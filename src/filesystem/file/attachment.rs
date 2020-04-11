use alloc::collections::VecDeque;
use alloc::prelude::v1::*;
use hashbrown::HashMap;

use d7abi::fs::protocol::attachment::*;
use d7abi::fs::FileDescriptor;

use crate::multitasking::{ExplicitEventId, FdWaitFlag, ProcessId, WaitFor};

use super::super::{error::IoResult, node::NodeId, FileClientId};
use super::{CloseAction, FileOps, Leafness, Trigger};

/// # Attachment point
/// This node and its contents are managed by a driver
/// software. On branch nodes, the driver can provide
/// child nodes that are used. The nodes themselves,
/// however, must exist before a read is attempted.
/// ## Nesting attach points
/// Nested mounts are allowed.
/// The innermost mount point will receive all operations.
/// ## Unattaching
/// Unlike Linux, where unmounting requires that all inner
/// mounts are unmounted first, this implementation simply
/// allows unattaching the point, causing all inner attachments
/// to be closed as well.
#[derive(Debug)]
pub struct Attachment {
    /// Process and file descriptor managing this attachment
    manager: FileClientId,
    /// Leafness is a static property of an attachment,
    /// the controlling process cannot change this
    is_leaf: bool,

    /// Marks whether manager can immediately read this
    manager_pending_data: FdWaitFlag,

    /// New read requests waiting for the manager to handle them.
    /// This is used store client wakeup ids.
    reads_pending: VecDeque<(FileClientId, ExplicitEventId)>,

    /// Reads in progress, waiting for the manager to write to them.
    /// This is used store client wakeup ids.
    reads_in_progress: HashMap<FileClientId, ExplicitEventId>,

    /// Completed reads. Data is removed when read. When all data has been removed,
    /// and the queue is empty, the entry here will be kept to until the client closes the file.
    reads_completed: HashMap<FileClientId, VecDeque<u8>>,

    /// Clients that have closed their file descriptor,
    /// waiting to be sent to the manager
    closed_pending: VecDeque<FileClientId>,
}
impl Attachment {
    pub fn new(manager: FileClientId, is_leaf: bool) -> Self {
        Self {
            manager,
            is_leaf,
            manager_pending_data: FdWaitFlag::new_unavailable(),
            reads_pending: VecDeque::new(),
            reads_in_progress: HashMap::new(),
            reads_completed: HashMap::new(),
            closed_pending: VecDeque::new(),
        }
    }

    /// Manager: process closed read client
    fn manager_read_close(&mut self, fc: FileClientId, buf: &mut [u8]) -> Option<IoResult<usize>> {
        debug_assert!(fc == self.manager);

        if let Some(closed_fc) = self.closed_pending.pop_front() {
            // The next client is trying to read
            let req = Request {
                sender: Sender {
                    pid: closed_fc.process.expect("TODO? kernel"),
                    f: unsafe { closed_fc.fd.as_u64() },
                },
                data: FileOperation::Close,
            };

            let bytes = pinecone::to_vec(&req).expect("Could not serialize");
            if bytes.len() <= buf.len() {
                buf[..bytes.len()].copy_from_slice(&bytes);
            } else {
                // TODO: Process error, not kernel panic
                panic!("Target buffer not large enough");
            }
            Some(IoResult::Success(bytes.len()))
        } else {
            None
        }
    }

    /// Manager: process pending read
    fn manager_read_pending(
        &mut self, fc: FileClientId, buf: &mut [u8],
    ) -> Option<IoResult<usize>> {
        debug_assert!(fc == self.manager);

        // Reading from attachment fd
        if let Some((reader_fc, event_id)) = self.reads_pending.pop_front() {
            // The next client is trying to read
            let req = Request {
                sender: Sender {
                    pid: reader_fc.process.expect("TODO? kernel"),
                    f: unsafe { reader_fc.fd.as_u64() },
                },
                data: FileOperation::Read(buf.len() as u64),
            };

            let bytes = pinecone::to_vec(&req).expect("Could not serialize");
            if bytes.len() <= buf.len() {
                buf[..bytes.len()].copy_from_slice(&bytes);
            } else {
                // TODO: Process error, not kernel panic
                panic!("Target buffer not large enough");
            }
            // Mark the read to be in progress
            self.reads_in_progress.insert(reader_fc, event_id);
            Some(IoResult::Success(bytes.len()))
        } else {
            None
        }
    }

    fn manager_read(&mut self, fc: FileClientId, buf: &mut [u8]) -> IoResult<usize> {
        assert!(fc == self.manager);
        log::trace!("Manager read");

        let action = self
            .manager_read_close(fc, buf)
            .or_else(|| self.manager_read_pending(fc, buf));

        if let Some(result) = action {
            log::debug!("Manager read complete more={:?}", self.has_pending_input());
            if !self.has_pending_input() {
                self.manager_pending_data.set_unavailable();
            }
            result
        } else {
            // No reads pending, wait until some other process tries to read
            let wait = self.manager_pending_data.expect_wait();
            IoResult::RepeatAfter(wait)
        }
    }

    fn has_pending_input(&self) -> bool {
        !(self.reads_pending.is_empty() && self.closed_pending.is_empty())
    }
}
impl FileOps for Attachment {
    fn leafness(&self) -> Leafness {
        if self.is_leaf {
            Leafness::Leaf
        } else {
            Leafness::Branch
        }
    }

    fn read(&mut self, fc: FileClientId, buf: &mut [u8]) -> IoResult<usize> {
        if fc == self.manager {
            self.manager_read(fc, buf)
        } else if let Some(mut data) = self.reads_completed.remove(&fc) {
            let mut i = 0;
            while i < buf.len() {
                if let Some(byte) = data.pop_front() {
                    buf[i] = byte;
                    i += 1;
                } else {
                    break;
                }
            }
            // Insert back
            self.reads_completed.insert(fc, data);
            // Return
            IoResult::Success(i)
        } else {
            // New read operation
            let event_id = WaitFor::new_event_id();
            self.reads_pending.push_back((fc, event_id));

            log::trace!("Creating new read operation + wait {:?}", event_id);
            log::trace!("Asking client to repeat request after the event");

            let repeat = IoResult::RepeatAfter(WaitFor::Event(event_id));
            self.manager_pending_data.set_available(repeat)
        }
    }

    fn read_waiting_for(&mut self, fc: FileClientId) -> WaitFor {
        if fc == self.manager {
            self.manager_pending_data.wait()
        } else if let Some((_, event_id)) = self
            .reads_pending
            .iter()
            .find(|(pending_fc, _)| *pending_fc == fc)
        {
            WaitFor::Event(*event_id)
        } else {
            WaitFor::None
        }
    }

    fn write(&mut self, fc: FileClientId, buf: &[u8]) -> IoResult<usize> {
        if fc == self.manager {
            // Manager writes response to a request. The whole response must be written at once.
            let (request, rest): (Response, &[u8]) =
                pinecone::take_from_bytes(buf).expect("Partial write from manager");

            let client_fc = FileClientId::process(request.sender.pid, unsafe {
                FileDescriptor::from_u64(request.sender.f)
            });

            let client_wakeup_event = self
                .reads_in_progress
                .remove(&client_fc)
                .expect("Client does not exist");

            self.reads_completed.insert(client_fc, request.data.into());
            IoResult::TriggerEvent(
                client_wakeup_event,
                Box::new(IoResult::Success(buf.len() - rest.len())),
            )
        } else {
            // Writes to attachments must be of type `d7abi::fs::protocol::attachment::Request`,
            // and the whole request must be written at once

            // let (req, rest): (Request, &[u8]) = pinecone::take_from_bytes(buf).except("Partial read");
            // panic!("R {:?}", req);
            // Ok(buf.len() - rest.len())
            unimplemented!()
        }
    }

    /// When manager closes the file, destroy this, triggering all waiting processes
    /// When client closes the file, send close message
    fn close(&mut self, fc: FileClientId) -> IoResult<CloseAction> {
        if fc == self.manager {
            return IoResult::Success(CloseAction::Destroy);
        }

        // Remove all ongoing reads for this client
        for (i, (fc_id, _event)) in self.reads_pending.iter().copied().enumerate() {
            if fc_id == fc {
                self.reads_pending.remove(i);
                break;
            }
        }

        if self.reads_in_progress.contains_key(&fc) {
            self.reads_in_progress.remove(&fc);
        } else if self.reads_completed.contains_key(&fc) {
            self.reads_completed.remove(&fc);
        }

        // Inform the manager about close
        self.closed_pending.push_back(fc);
        self.manager_pending_data
            .set_available(IoResult::Success(CloseAction::Normal))
    }

    /// Trigger all waiting processes
    fn destroy(&mut self) -> Trigger {
        Trigger::events(
            self.reads_pending
                .iter()
                .map(|(_, w)| w)
                .chain(self.reads_in_progress.values())
                .copied()
                .collect(),
        )
    }
}

use alloc::prelude::v1::*;
use core::convert::TryInto;
use hashbrown::HashMap;
use lazy_static::lazy_static;
use serde::Serialize;
use spin::Mutex;

use d7abi::fs::{FileDescriptor, FileInfo};

use crate::memory::MemoryController;
use crate::multitasking::{
    process::{ProcessId, ProcessResult},
    ElfImage, Scheduler, WaitFor,
};

pub mod file;
mod node;
mod path;
pub mod result;
pub mod staticfs;

use self::file::*;
use self::result::{ErrorCode, IoContext, IoResult, IoResultPure};

pub use self::node::*;
pub use self::path::{Path, PathBuf};

const ROOT_ID: NodeId = NodeId::first();

/// Globally unique file descriptor
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileClientId {
    /// `None` here repensents that the operation
    /// was initiated by the kernel, and not any process.
    pub process: Option<ProcessId>,
    /// Per-process file descriptor
    pub fd: FileDescriptor,
}
impl FileClientId {
    fn kernel(fd: FileDescriptor) -> Self {
        Self { process: None, fd }
    }

    pub fn process(pid: ProcessId, fd: FileDescriptor) -> Self {
        Self {
            process: Some(pid),
            fd,
        }
    }

    pub fn is_kernel(&self) -> bool {
        self.process.is_none()
    }
}

/// NodeId and descriptors for a process
struct ProcessDescriptors {
    /// Node id of the process
    node_id: NodeId,
    /// Descriptors owned by the process
    descriptors: HashMap<FileDescriptor, NodeId>,
    /// Next available file descriptor
    next_fd: FileDescriptor,
}
impl ProcessDescriptors {
    fn new(node_id: NodeId) -> Self {
        Self {
            node_id,
            descriptors: HashMap::new(),
            next_fd: unsafe { FileDescriptor::from_u64(0) },
        }
    }

    fn resolve(&self, fd: FileDescriptor) -> Option<NodeId> {
        self.descriptors.get(&fd).copied()
    }

    fn create_id(&mut self, node_id: NodeId) -> FileDescriptor {
        let fd = self.next_fd;
        self.descriptors.insert(fd, node_id);
        self.next_fd = unsafe { self.next_fd.next() };
        fd
    }
}

/// Read all bytes. Used when the kernel needs to read all data from a file.
/// # Safety
/// This is marked unsafe, as it discards data if an io error occurs during it.
unsafe fn read_to_end(file: &mut dyn FileOps, fc: FileClientId) -> IoResult<Vec<u8>> {
    const IO_BUFFER_SIZE: usize = 4096;

    let mut result = Vec::new();
    let mut buffer = [0u8; IO_BUFFER_SIZE];
    loop {
        let (count, events) = file.read(fc, &mut buffer)?;
        assert!(events.is_empty()); // TODO
        result.extend(buffer[..count].iter());
        if count < IO_BUFFER_SIZE {
            // EOF
            break;
        }
    }
    IoResult::success(result)
}

/// Write all bytes. Used when the kernel needs to write a fixed amount of data.
/// # Safety
/// This is marked unsafe, as it discards data if an io error occurs during it.
unsafe fn write_all(file: &mut dyn FileOps, fc: FileClientId, data: &[u8]) -> IoResult<()> {
    const IO_BUFFER_SIZE: usize = 4096;

    let mut offset: usize = 0;
    let mut ctx = IoContext::new();
    while offset < data.len() {
        let (count, new_ctx) = file.write(fc, &data[offset..]).with_context(ctx)?;
        ctx = new_ctx;
        offset += count;
        if count == 0 {
            panic!("Write failed");
        }
    }
    IoResult::success(()).with_context(ctx)
}

/// Serialize with Pinecone, and write all bytes
unsafe fn write_all_ser<T: Serialize>(
    file: &mut dyn FileOps, fc: FileClientId, data: &T,
) -> IoResult<()> {
    let data = pinecone::to_vec(data).expect("Failed to encode");
    write_all(file, fc, &data)
}

pub struct VirtualFS {
    nodes: HashMap<NodeId, Node>,
    descriptors: HashMap<ProcessId, ProcessDescriptors>,
    next_node_id: NodeId,
    next_kernel_fd: FileDescriptor,
}
impl VirtualFS {
    pub fn new() -> Self {
        let mut nodes = HashMap::new();
        let mut root_node = Node::new(None, Box::new(InternalBranch::new()));
        root_node.inc_ref(); // Root node refers to "itself", and will not be removed
        nodes.insert(ROOT_ID, root_node);
        Self {
            nodes,
            descriptors: HashMap::new(),
            next_node_id: ROOT_ID.next(),
            next_kernel_fd: unsafe { FileDescriptor::from_u64(0) },
        }
    }

    pub fn take_kernel_fc(&mut self) -> FileClientId {
        let fd = self.next_kernel_fd;
        self.next_kernel_fd = unsafe { self.next_kernel_fd.next() };
        FileClientId::kernel(fd)
    }

    /// Traverse tree and return NodeId
    fn resolve(&mut self, path: &Path) -> IoResult<NodeId> {
        assert!(path.is_absolute());
        let mut cursor: NodeId = ROOT_ID;
        let mut ctx = IoContext::new();
        for c in path.components() {
            let (new_cursor, new_ctx) = self.get_child(cursor, c).with_context(ctx)?;
            cursor = new_cursor;
            ctx = new_ctx;
        }
        IoResult::success(cursor).with_context(ctx)
    }

    /// Node ancestry in order [node, ..., root], or None if not connected to root
    fn node_ancestors(&self, node_id: NodeId) -> IoResult<Option<Vec<NodeId>>> {
        let mut ancestry = vec![node_id];
        loop {
            let node = self.node(*ancestry.last().unwrap())?;
            if let Some(parent) = node.parent {
                ancestry.push(parent);
            } else {
                break;
            }
        }
        IoResult::success(if *ancestry.last().unwrap() == ROOT_ID {
            Some(ancestry)
        } else {
            None
        })
    }

    /// Resolve path from node, if any
    /// This is only for creating debug strings, as it can return None even when a path exists
    pub fn debug_node_id_to_path(&mut self, node_id: NodeId) -> Option<PathBuf> {
        let ancestors = self.node_ancestors(node_id);
        if !ancestors.is_success() {
            return None;
        }
        let mut ancestors = ancestors.unwrap()?;
        let mut pb = Path::new("/").to_path_buf();
        'outer: loop {
            let parent_id = ancestors.pop().unwrap();
            if let Some(child_id) = ancestors.last().copied() {
                let r = self.get_children(parent_id);
                if !r.is_success() {
                    // path blocked by non-resolving node (possibly an attachment)
                    return None;
                }
                for (id, name) in r.unwrap() {
                    if id == child_id {
                        pb.push(&name);
                        continue 'outer;
                    }
                }
                todo!("???"); // FIXME: remove this?
            // IoResult::error(ErrorCode::fs_node_not_found)
            } else {
                break;
            }
        }
        Some(pb)
    }

    pub fn node(&self, id: NodeId) -> IoResultPure<&Node> {
        if let Some(node) = self.nodes.get(&id) {
            IoResultPure::Success(node)
        } else {
            IoResultPure::Error(ErrorCode::fs_file_destroyed)
        }
    }

    pub fn node_mut(&mut self, id: NodeId) -> IoResultPure<&mut Node> {
        if let Some(node) = self.nodes.get_mut(&id) {
            IoResultPure::Success(node)
        } else {
            IoResultPure::Error(ErrorCode::fs_file_destroyed)
        }
    }

    /// Create a new node
    pub fn create_node(&mut self, parent: NodeId, name: &str, new_node: Node) -> IoResult<NodeId> {
        self.create_node_with(parent, name, |_, _| (new_node, ()))
            .map(|t| t.0)
    }

    /// Create a new node, with contents created using a closure
    pub fn create_node_with<F, R>(
        &mut self, parent: NodeId, name: &str, f: F,
    ) -> IoResult<(NodeId, R)>
    where F: FnOnce(&mut Self, NodeId) -> (Node, R) {
        let new_node_id = self.next_node_id;
        let (exists, mut ctx) = self.has_child(parent, name)?;
        if exists {
            IoResult::error(ErrorCode::fs_node_exists).with_context(ctx)
        } else if self.node(parent)?.leafness() == Leafness::Leaf {
            IoResult::error(ErrorCode::fs_node_is_leaf).with_context(ctx)
        } else {
            let (_, ctx) = self
                .add_child(parent, name, new_node_id)
                .with_context(ctx)?;
            let (new_node, result) = f(self, new_node_id);
            self.nodes.insert(new_node_id, new_node);
            self.next_node_id = new_node_id.next();
            IoResult::success((new_node_id, result)).with_context(ctx)
        }
    }

    /// Create a new anonymous node
    pub fn create_anon_node(&mut self, new_node: Node) -> NodeId {
        self.create_anon_node_with(|_, _| (new_node, ())).0
    }

    /// Create a new anonymous node, with contents created using a closure
    pub fn create_anon_node_with<F, R>(&mut self, f: F) -> (NodeId, R)
    where F: FnOnce(&mut Self, NodeId) -> (Node, R) {
        let new_node_id = self.next_node_id;
        let (new_node, result) = f(self, new_node_id);
        self.nodes.insert(new_node_id, new_node);
        self.next_node_id = new_node_id.next();
        (new_node_id, result)
    }

    /// Create a new node, and give it one initial reference so it
    /// will not be removed automatically
    fn create_static(&mut self, path: &Path, dev: Box<dyn FileOps>) -> IoResultPure<NodeId> {
        let parent_path = path.parent().expect("Path without parent?");
        let parent = self
            .resolve(parent_path)
            .expect_events("create_static must not cause events")?;
        let file_name = path.file_name().expect("Path without file_name?");
        let id = self
            .create_node(parent, file_name, Node::new(Some(parent), dev))
            .expect_events("create_static must not cause events")?;
        self.node_mut(id).unwrap().inc_ref();
        IoResultPure::Success(id)
    }

    /// Create a new branch node, and give it one initial reference so it
    /// will not be removed automatically
    pub fn create_static_branch(&mut self, path: &Path) -> IoResultPure<NodeId> {
        self.create_static(path, Box::new(InternalBranch::new()))
    }

    pub fn get_children(&mut self, node_id: NodeId) -> IoResult<Vec<(NodeId, String)>> {
        match self.node(node_id)?.leafness() {
            Leafness::Leaf => IoResult::error(ErrorCode::fs_node_is_leaf),
            Leafness::Branch => {
                use d7abi::fs::protocol::attachment::ReadAttachmentBranch;
                // Use `ReadAttachmentBranch` protocol.
                // Note that this might result in event + repeat after request
                let fc = self.take_kernel_fc();
                let node = self.node_mut(node_id)?;
                log::trace!("get_children (attachment)");
                let mut ctx = IoContext::new();
                let pid = node.data.pid()?;
                let (bytes, ctx) = unsafe { read_to_end(&mut *node.data, fc) }.with_context(ctx)?;
                let data: ReadAttachmentBranch = pinecone::from_bytes(&bytes)
                    .expect("Failed to deserialize ReadAttachmentBranch protocol message");

                log::trace!("D {:?}", data); // TODO

                let mut result = Vec::new();
                for (name, fd) in data.items {
                    let node_id = self.resolve_fc(FileClientId {
                        process: Some(pid),
                        fd,
                    })?;
                    result.push((node_id, name));
                }
                IoResult::success(result).with_context(ctx)
            },
            Leafness::InternalBranch => {
                // Use internal branch protocol.
                let (bytes, ctx) = self.temp_open(node_id, |node, fc| unsafe {
                    read_to_end(&mut *node.data, fc)
                })?;
                IoResult::success(
                    pinecone::from_bytes(&bytes)
                        .expect("Failed to deserialize InternalBranch protocol message"),
                )
                .with_context(ctx)
            },
        }
    }

    pub fn get_child(&mut self, node_id: NodeId, name: &str) -> IoResult<NodeId> {
        let (children, ctx) = self.get_children(node_id)?;
        if let Some(r) = children
            .iter()
            .find(|(_, n_name)| n_name == name)
            .map(|(id, _)| id)
            .copied()
        {
            IoResult::success(r)
        } else {
            IoResult::error(ErrorCode::fs_node_not_found)
        }
    }

    pub fn has_child(&mut self, node_id: NodeId, name: &str) -> IoResult<bool> {
        let (child, ctx) = self.get_child(node_id, name).separate_events();
        IoResult::success(match child {
            IoResultPure::Success(_) => true,
            IoResultPure::Error(ErrorCode::fs_node_not_found) => false,
            other => todo!("has_child {:?}", other),
        })
        .with_context(ctx)
    }

    /// Adds a child. This does not work with non-internal branches;
    /// they must be modified by userspace processes only.
    fn add_child(&mut self, parent_id: NodeId, child_name: &str, child_id: NodeId) -> IoResult<()> {
        match self.node(parent_id)?.leafness() {
            Leafness::Leaf => IoResult::error(ErrorCode::fs_node_is_leaf),
            Leafness::Branch => panic!("add_child only supports internal branches"),
            Leafness::InternalBranch => {
                // Use internal branch protocol
                let data = InternalModification::Add(child_id, child_name.to_owned());
                self.temp_open(parent_id, |node, fc| unsafe {
                    write_all(
                        &mut *node.data,
                        fc,
                        &pinecone::to_vec(&data).expect("Failed to encode"),
                    )
                })
            },
        }
    }

    /// Removes a child. This does not work with non-internal branches;
    /// they must be modified by userspace processes only.
    fn remove_child(&mut self, parent_id: NodeId, child_id: NodeId) -> IoResultPure<()> {
        match self.node(parent_id)?.leafness() {
            Leafness::Leaf => IoResultPure::Error(ErrorCode::fs_node_is_leaf),
            Leafness::Branch => panic!("remove_child only supports internal branches"),
            Leafness::InternalBranch => {
                // Use internal branch protocol
                let data = InternalModification::Remove(child_id);
                self.temp_open(parent_id, |node, fc| unsafe {
                    write_all(
                        &mut *node.data,
                        fc,
                        &pinecone::to_vec(&data).expect("Failed to encode"),
                    )
                })
                .expect_events("remove_child doesn't support events")
            },
        }
    }

    /// Temporarily open a file
    fn temp_open<F, R>(&mut self, node_id: NodeId, body: F) -> IoResult<R>
    where F: FnOnce(&mut Node, FileClientId) -> IoResult<R> {
        let fc = self.take_kernel_fc();
        let node = self
            .nodes
            .get_mut(&node_id)
            .expect("temp_open: node missing");
        let ctx = IoContext::new();
        let (_, mut ctx) = node.open(fc).with_context(ctx)?;
        let result: IoResult<R> = body(node, fc);
        let (keep, new_ctx) = node.close(fc).separate_events();
        ctx.consume(new_ctx);
        let keep = keep.expect("close must not fail (temp_open)");
        assert!(keep == CloseAction::Normal, "Not possible");
        result.with_context(ctx)
    }

    /// Get process descriptors
    fn process(&self, pid: ProcessId) -> &ProcessDescriptors {
        self.descriptors.get(&pid).expect("No such process")
    }

    /// Get process descriptors mutably
    fn process_mut(&mut self, pid: ProcessId) -> &mut ProcessDescriptors {
        self.descriptors.get_mut(&pid).expect("No such process")
    }

    /// File info (system call)
    pub fn fileinfo(&mut self, path: &str) -> IoResultPure<FileInfo> {
        let path = Path::new(path);
        let id = self
            .resolve(path)
            .expect_events("TODO: support fs_fileinfo events")?;
        IoResultPure::Success(self.node(id)?.fileinfo())
    }

    /// Open a file (system call)
    pub fn open(
        &mut self, sched: &mut Scheduler, pid: ProcessId, path: &str,
    ) -> IoResultPure<FileClientId> {
        let path = Path::new(path);
        let node_id = self.resolve(path.clone()).consume_events(sched)?;
        let process = self.process_mut(pid);
        let fd = process.create_id(node_id);
        let fc = FileClientId::process(pid, fd);
        self.node_mut(node_id)?.open(fc).consume_events(sched)?;
        IoResultPure::Success(fc)
    }

    /// Exec a file (system call)
    pub fn exec(
        &mut self, mem_ctrl: &mut MemoryController, sched: &mut Scheduler, owner_pid: ProcessId,
        path: &str,
    ) -> IoResultPure<FileClientId>
    {
        self.exec_optional_owner(mem_ctrl, sched, path, Some(owner_pid))
            .consume_events(sched)
    }

    /// Exec a file without a parent process, i.e. init
    pub fn kernel_exec(
        &mut self, mem_ctrl: &mut MemoryController, sched: &mut Scheduler, path: &str,
    ) -> IoResult<FileClientId> {
        self.exec_optional_owner(mem_ctrl, sched, path, None)
    }

    fn exec_optional_owner(
        &mut self, mem_ctrl: &mut MemoryController, sched: &mut Scheduler, path: &str,
        owner_pid: Option<ProcessId>,
    ) -> IoResult<FileClientId>
    {
        // Load elf image
        let elfimage = self.load_module(mem_ctrl, path).consume_events(sched)?;

        // Open the executable file for the owner of the process
        let path = Path::new(path);
        let owner_node_id = self.resolve(path).consume_events(sched)?;
        let fc = {
            if let Some(pid) = owner_pid {
                let process = self.process_mut(pid);
                let fd = process.create_id(owner_node_id);
                FileClientId::process(pid, fd)
            } else {
                self.take_kernel_fc()
            }
        };
        self.node_mut(owner_node_id)?.inc_ref();

        // Spawn the new process
        let new_pid = unsafe { sched.spawn(mem_ctrl, elfimage) };

        // Create a node and descriptors for the process
        let directory_id = self.resolve(Path::new("/prc")).expect("/prc missing");
        let node_id = self
            .create_node(
                directory_id,
                &new_pid.to_string(),
                Node::new(Some(directory_id), Box::new(ProcessFile::new(new_pid, fc))),
            )
            .consume_events(sched)?;
        self.descriptors
            .insert(new_pid, ProcessDescriptors::new(node_id));

        // Open a file descriptor to the process for the owner
        let fc = {
            if let Some(pid) = owner_pid {
                let process = self.process_mut(pid);
                let fd = process.create_id(node_id);
                FileClientId::process(pid, fd)
            } else {
                self.take_kernel_fc()
            }
        };
        self.node_mut(node_id)?
            .open(fc)
            .expect("Unable to open node");
        IoResult::success(fc)
    }

    /// Loads elf image to ram and returns it
    pub fn load_module(
        &mut self, mem_ctrl: &mut MemoryController, path: &str,
    ) -> IoResult<ElfImage> {
        use core::ptr;
        use x86_64::structures::paging::PageTableFlags as Flags;

        use crate::memory::prelude::*;
        use crate::memory::Area;
        use crate::memory::{self, Page};

        let bytes = self
            .read_file(path)
            .expect_events("load_module doesn't support events yet")?;

        let size_pages =
            memory::page_align(PhysAddr::new(bytes.len() as u64), true).as_u64() / Page::SIZE;

        // Allocate load buffer
        let area = mem_ctrl.alloc_pages(size_pages as usize, Flags::PRESENT | Flags::WRITABLE);

        // Store the file to buffer
        let base: *mut u8 = area.start.as_mut_ptr();
        let mut it = bytes.into_iter();
        for page_offset in 0..size_pages {
            for byte_offset in 0..PAGE_SIZE_BYTES {
                let i = page_offset * PAGE_SIZE_BYTES + byte_offset;
                unsafe {
                    ptr::write(base.add(i as usize), it.next().unwrap_or(0));
                }
            }
        }

        let elf = unsafe { ElfImage::new(area) };
        elf.verify();
        IoResult::success(elf)
    }

    /// Resolves file descriptor for a process
    pub fn resolve_fc(&mut self, fc: FileClientId) -> IoResultPure<NodeId> {
        IoResultPure::Success(
            self.process(fc.process.expect("Kernel not supported here"))
                .resolve(fc.fd)
                .expect("No such file descriptor for process"),
        )
    }

    /// Attach to fs (system call)
    /// Give empty path to not bind the attachment directly to VFS,
    /// this is used to create subnodes for attached branches
    pub fn attach(
        &mut self, sched: &mut Scheduler, pid: ProcessId, path: &str, is_leaf: bool,
    ) -> IoResultPure<FileClientId> {
        let (node_id, fc) = if !path.is_empty() {
            // Create with an attachment point
            let path = Path::new(path);
            let parent = self
                .resolve(path.parent().expect("No parent?"))
                .consume_events(sched)?;
            let file_name = path.file_name().expect("No file_name?");
            self.create_node_with(parent, file_name, |s, id| {
                let process = s.process_mut(pid);
                let fd = process.create_id(id);
                let fc = FileClientId::process(pid, fd);
                (
                    Node::new(Some(parent), Box::new(Attachment::new(fc, is_leaf))),
                    fc,
                )
            })
            .consume_events(sched)?
        } else {
            // Create unattached
            self.create_anon_node_with(|s, id| {
                let process = s.process_mut(pid);
                let fd = process.create_id(id);
                let fc = FileClientId::process(pid, fd);
                (Node::new(None, Box::new(Attachment::new(fc, is_leaf))), fc)
            })
        };

        // Create new fd for the attachment
        self.node_mut(node_id)?.open(fc).consume_events(sched)?;
        IoResultPure::Success(fc)
    }

    /// Close a file descriptor (system call)
    /// If the process owning the file descriptor is already
    /// dead, then the close request is simply ignored
    pub fn close(&mut self, sched: &mut Scheduler, fc: FileClientId) -> IoResultPure<()> {
        let fc_pid = fc.process.expect("Kernel not supported here");
        let pd = self
            .descriptors
            .get(&fc_pid)
            .unwrap_or_else(|| panic!("No such process {}", fc_pid));

        let node_id = pd.resolve(fc.fd).expect("No such file descriptor");
        self.close_removed(sched, fc, node_id).consume_events(sched)
    }

    /// Close a file descriptor using node_id
    /// Can be used to close file descriptors for already-killed process.
    /// This function never fails, but may trigger events using `IoResult`.
    pub fn close_removed(
        &mut self, sched: &mut Scheduler, fc: FileClientId, node_id: NodeId,
    ) -> IoResult<()> {
        if let Some(node) = self.nodes.get_mut(&node_id) {
            let action = node.close(fc).consume_events(sched);
            let action = action.expect("Close is not allowed to fail");
            if action == CloseAction::Destroy {
                let mut node = self.remove_node(node_id);
                let trigger = node.data.destroy();
                trigger.run(sched, self);
            }
        }
        IoResult::success(())
    }

    /// Read from file (system call)
    pub fn read(
        &mut self, sched: &mut Scheduler, fc: FileClientId, buf: &mut [u8],
    ) -> IoResultPure<usize> {
        let node_id = self.resolve_fc(fc)?;
        let node = self.node_mut(node_id)?;
        node.read(fc, buf).consume_events(sched)
    }

    /// Read waiting for (used by system call: fd_select)
    pub fn read_waiting_for(
        &mut self, sched: &mut Scheduler, fc: FileClientId,
    ) -> IoResultPure<WaitFor> {
        let node_id = self.resolve_fc(fc)?;
        IoResultPure::Success((*self.node_mut(node_id)?.data).read_waiting_for(fc))
    }

    /// Write to file (system call)
    pub fn write(
        &mut self, sched: &mut Scheduler, fc: FileClientId, buf: &[u8],
    ) -> IoResultPure<usize> {
        let node_id = self.resolve_fc(fc)?;
        let node = self.node_mut(node_id)?;
        node.write(fc, buf).consume_events(sched)
    }

    /// Get pid (system call)
    pub fn get_pid(&mut self, fc: FileClientId) -> IoResultPure<ProcessId> {
        let node_id = self.resolve_fc(fc)?;
        let node = self.node(node_id)?;
        node.data.pid()
    }

    /// Read a file for other kernel component
    /// Discards data on error.
    pub fn read_file(&mut self, path: &str) -> IoResult<Vec<u8>> {
        let (node_id, events) = self.resolve(Path::new(path))?;
        assert!(events.is_empty()); // TODO: handle events
        self.temp_open(node_id, |node, fc| unsafe {
            read_to_end(&mut *node.data, fc)
        })
    }

    /// Remove a closed node
    #[must_use]
    pub fn remove_node(&mut self, node_id: NodeId) -> Node {
        // Remove the node itself
        let node = self.nodes.remove(&node_id).expect("Node does not exist");

        // Remove link from parent branch
        if let Some(parent) = node.parent {
            let result = self.remove_child(parent, node_id);
            match result {
                IoResultPure::Success(()) => {},
                IoResultPure::Error(ErrorCode::fs_file_destroyed) => {},
                error => panic!("Child removal failed: {:?}", error),
            };
        }

        node
    }

    /// Update when a process completes.
    /// Closes all files opened by the process, and send wakeup signals for them if required.
    /// TODO: flush/synchronize buffers?
    pub fn on_process_over(
        &mut self, sched: &mut Scheduler, pid: ProcessId, status: ProcessResult,
    ) {
        if let Some(pd) = self.descriptors.remove(&pid) {
            // Inform process about its status.
            // The node will not exist iff all process having access to
            // it's result are already dead. In this case the status
            // doesn't have to be stored
            if self.nodes.contains_key(&pd.node_id) {
                self.temp_open(pd.node_id, |node, fc| unsafe {
                    write_all_ser(&mut *node.data, fc, &status)
                })
                .expect("FileOps::close not supported for Process files")
            }

            // Close files process was holding open
            for (fd, node_id) in pd.descriptors.into_iter() {
                let fc = FileClientId::process(pid, fd);
                self.close_removed(sched, fc, node_id)
                    .consume_events(sched)
                    .expect("Closing a file must not fail (on_process_over)");
            }
        }
    }
}

lazy_static! {
    pub static ref FILESYSTEM: Mutex<VirtualFS> = Mutex::new(VirtualFS::new());
}

fn create_fs(fs: &mut VirtualFS) -> IoResult<()> {
    // Create top-level fs hierarchy
    fs.create_static_branch(Path::new("/bin"))?; // Binaries
    fs.create_static_branch(Path::new("/cfg"))?; // System configuration
    fs.create_static_branch(Path::new("/dev"))?; // Device files
    fs.create_static_branch(Path::new("/mnt"))?; // Fs mount points
    fs.create_static_branch(Path::new("/prc"))?; // Processes
    fs.create_static_branch(Path::new("/srv"))?; // Service endpoints

    // Insert special files
    fs.create_static(Path::new("/dev/null"), Box::new(NullDevice))?;
    fs.create_static(Path::new("/dev/zero"), Box::new(ZeroDevice))?;
    fs.create_static(Path::new("/dev/test"), Box::new(TestDevice { rounds: 3 }))?;

    // Kernel console
    fs.create_static(
        Path::new("/dev/console"),
        Box::new(KernelConsoleDevice::new()),
    )?;

    // NIC interface
    fs.create_static(Path::new("/dev/nic"), Box::new(NetworkDevice))?;
    fs.create_static(Path::new("/dev/nic_mac"), Box::new(MacAddrDevice))?;

    IoResult::success(())
}

pub fn init() {
    let mut fs = FILESYSTEM.lock();
    create_fs(&mut *fs).expect("Could not init filesystem");
}

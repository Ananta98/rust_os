; RUSTOS LOADER
; STAGE 1


; Kernel elf executable initial load point
%define loadpoint 0x8000

; Page tables
%define page_table_section_start 0x00020000
%define page_table_p4 0x00020000
%define page_table_p3 0x00021000
%define page_table_p2 0x00022000
%define page_table_section_end 0x00023000


[BITS 32]
[ORG 0x7e00]


%macro lp 0
mov dword [0xb8000 + 80*12 + 80], 0x0f4d0f4a
mov dword [0xb8000 + 80*12 + 84], 0x0f250f50
jmp $
%endmacro

stage1:
    ; SCREEN: top left: "11"
    mov dword [0xb8000], 0x2f312f31

    ; paging
    call set_up_page_tables
    call enable_paging


    ; parse elf header
    ; http://wiki.osdev.org/ELF#Tables
    ;
    ; Because we are working in protected mode, we assume some values to fit in 32 bits.
    ; Of course we test thay they are, but this code gives error if they aren't
    ; It's not good practice, but... here we go :]
    ;
    ; elf error messages begin with "E"
    mov al, 'E'

    ; magic number 0x7f+'ELF'
    ; if not elf show error message "E!"
    mov ah, '!'
    cmp dword [loadpoint + 0], 0x464c457f
    jne error

    ; bitness and instrucion set (must be 64, so values must be 2 and 0x3e) (error code: "EB")
    mov ah, 'B'
    cmp byte [loadpoint + 4], 0x2
    jne error
    cmp word [loadpoint + 18], 0x3e
    jne error

    ; endianess (must be little endian, so value must be 1) (error code: "EE")
    mov ah, 'E'
    cmp byte [loadpoint + 5], 0x1
    jne error

    ; elf version (must be 2) (error code: "EV")
    mov ah, 'V'
    cmp byte [loadpoint + 0x0006], 0x2

    ; Now lets trust it's actually real and valid elf file

    ; kernel entry position must be 0x_00000000_00010000
    ; (error code : "EP")
    mov ah, 'P'
    cmp dword [loadpoint + 24], 0x00010000
    jne error
    cmp dword [loadpoint + 28], 0x00000000
    jne error

    ; load point is correct, great. print green OK
    mov dword [0xb8000 + 80*24], 0x2f4b2f4f


    ; Parse program headers and relocate sections
    ; http://wiki.osdev.org/ELF#Program_header
    ; (error code : "EH")
    mov ah, 'H'

    ; We know that program header size is 56 (=0x38) bytes
    ; still, lets check it:
    cmp word [loadpoint + 54], 0x38
    jne error


    ; get "Program header table position", check that it is max 32bits
    mov ebx, dword [loadpoint + 32]
    cmp dword [loadpoint + 36], 0x00000000
    jne error
    add ebx, loadpoint ; now ebx points to first program header

    ; get length of program header table
    mov ecx, 0
    mov cx, [loadpoint + 56]

    ; loop through headers
.loop_headers:
    ; First, lets check that this sector should be loaded

    mov word [0xb8040 + ecx*2], 0x0f2b  ; Print plus-chars to first line to show process

    cmp dword [ebx], 1 ; load: this is important
    jne .next   ; if not important: continue


    mov word [0xb8040 + ecx*2], 0x0f2a  ; Overwrite plus-chars with asterisks to first line to show process

    ; load: clear p_memsz bytes at p_vaddr to 0, then copy p_filesz bytes from p_offset to p_vaddr
    push ecx

    ; lets ignore some (probably important) stuff here
    ; Again, because we are working in protected mode, we assume some values to fit in 32 bits.

    ; esi = p_offset
    mov esi, [ebx + 8]
    cmp dword [ebx + 12], 0x00000000
    jne error
    add esi, loadpoint  ; now points to begin of buffer we must copy

    ; edi = p_vaddr
    mov edi, [ebx + 16]
    cmp dword [ebx + 20], 0x00000000
    jne error

    ; ecx = p_memsz
    mov ecx, [ebx + 40]
    cmp dword [ebx + 44], 0x00000000
    jne error

    ; <1> clear p_memsz bytes at p_vaddr to 0
    push edi
.loop_clear:
    mov byte [edi], 0
    inc edi
    loop .loop_clear
    pop edi
    ; </1>

    ; ecx = p_filesz
    mov ecx, [ebx + 32]
    cmp dword [ebx + 36], 0x00000000
    jne error

    ; <2> copy p_filesz bytes from p_offset to p_vaddr
    ; uses: esi, edi, ecx
    rep movsb   ; https://en.wikibooks.org/wiki/X86_Assembly/Data_Transfer#Move_String
    ; </2>

    pop ecx

    ; next entry
    loop .loop_headers

    ; no next entry, done
    jmp .over

.next:
    add ebx, 0x38   ; skip entry (0x38 is entry size)
    loop .loop_headers

    ; ELF relocation done
.over:
    ; going to byte bytes mode (8*8 = 2**6 = 64 bits = Long mode)

    ; load GDT
    lgdt [gdt64.pointer]

    ; Now we are in some kind of compatibility mode
    ; Don't do anything else that update selectors and jump
    ; (I think memory access will fail)

    ; update selectors
    mov dx, gdt64.data
    mov ss, dx  ; stack selector
    mov ds, dx  ; data selector
    mov es, dx  ; extra selector

    mov edx, 0x1000CAFE

    ; jump into kernel entry (relocated to 0x00010000)
    jmp gdt64.code:0x00010000

; set up paging
; http://os.phil-opp.com/entering-longmode.html#set-up-identity-paging
; http://wiki.osdev.org/Paging
; http://pages.cs.wisc.edu/~remzi/OSTEP/vm-paging.pdf
; Identity map first 1GiB (0x200000 * 0x200)
; using 2MiB pages
set_up_page_tables:
    ; map first P4 entry to P3 table
    mov eax, page_table_p3
    or eax, 0b11 ; present & writable
    mov [page_table_p4], eax

    ; map first P3 entry to P2 table
    mov eax, page_table_p2
    or eax, 0b11 ; present & writable
    mov [page_table_p3], eax

    ; map each P2 entry to a huge 2MiB page
    mov ecx, 0         ; counter

.map_page_table_p2_loop:
    ; map ecx-th P2 entry to a huge page that starts at address 2MiB*ecx
    mov eax, 0x200000                   ; 2MiB
    mul ecx                             ; page[ecx] start address
    or eax, 0b10000011                  ; present & writable & huge
    mov [page_table_p2 + ecx * 8], eax  ; map entry

    inc ecx
    cmp ecx, 0x200                  ; is the whole P2 table is mapped?
    jne .map_page_table_p2_loop     ; next entry
    ; done
    ret

; enable_paging
; http://os.phil-opp.com/entering-longmode.html#enable-paging
; http://wiki.osdev.org/Paging#Enabling
enable_paging:
    ; load P4 to cr3 register (cpu uses this to access the P4 table)
    mov eax, page_table_p4
    mov cr3, eax

    ; enable PAE-flag in cr4 (Physical Address Extension)
    mov eax, cr4
    or eax, 1 << 5
    mov cr4, eax

    ; set the long mode bit in the EFER MSR (model specific register)
    mov ecx, 0xC0000080
    rdmsr
    or eax, 1 << 8
    wrmsr

    ; enable paging in the cr0 register
    mov eax, cr0
    or eax, 1 << 31
    mov cr0, eax
    ret



; Prints `ERR: ` and the given 2-character error code to screen (TL) and hangs.
; args: ax=(al,ah)=error_code (2 characters)
error:
    mov dword [0xb8000 + 5 * 2*80], 0x4f524f45
    mov dword [0xb8004 + 5 * 2*80], 0x4f3a4f52
    mov dword [0xb8008 + 5 * 2*80], 0x4f204f20
    mov dword [0xb800a + 5 * 2*80], 0x4f204f20
    mov byte  [0xb800a + 5 * 2*80], al
    mov byte  [0xb800c + 5 * 2*80], ah
    hlt


; Constant data section

; GDT (Global Descriptor Table)
gdt64:
    dq 0 ; zero entry
.code: equ $ - gdt64
    dq (1<<44) | (1<<47) | (1<<41) | (1<<43) | (1<<53) ; code segment
.data: equ $ - gdt64
    dq (1<<44) | (1<<47) | (1<<41) ; data segment
.pointer:   ; pointer "struct"
    dw $ - gdt64 - 1
    dq gdt64


times (0x200-($-$$)) db 0 ; fill sector

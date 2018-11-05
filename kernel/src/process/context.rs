use arch::interrupt::{TrapFrame, Context as ArchContext};
use memory::{MemoryArea, MemoryAttr, MemorySet, KernelStack, active_table_swap, alloc_frame};
use xmas_elf::{ElfFile, header, program::{Flags, ProgramHeader, Type}};
use core::fmt::{Debug, Error, Formatter};
use ucore_process::Context;
use alloc::boxed::Box;
use ucore_memory::{Page};
use ::memory::{InactivePageTable0, memory_set_record};
use ucore_memory::memory_set::*;

pub struct ContextImpl {
    arch: ArchContext,
    memory_set: MemorySet,
    kstack: KernelStack,
}

impl Context for ContextImpl {
    unsafe fn switch_to(&mut self, target: &mut Context) {
        use core::mem::transmute;
        let (target, _): (&mut ContextImpl, *const ()) = transmute(target);
        self.arch.switch(&mut target.arch);
    }
}

impl ContextImpl {
    pub unsafe fn new_init() -> Box<Context> {
        Box::new(ContextImpl {
            arch: ArchContext::null(),
            memory_set: MemorySet::new(),
            kstack: KernelStack::new(),
        })
    }

    pub fn new_kernel(entry: extern fn(usize) -> !, arg: usize) -> Box<Context> {
        let memory_set = MemorySet::new();
        let kstack = KernelStack::new();
        Box::new(ContextImpl {
            arch: unsafe { ArchContext::new_kernel_thread(entry, arg, kstack.top(), memory_set.token()) },
            memory_set,
            kstack,
        })
    }

    /// Make a new user thread from ELF data
    /*
    * @param: 
    *   data: the ELF data stream 
    * @brief: 
    *   make a new thread from ELF data
    * @retval: 
    *   the new user thread Context
    */
    pub fn new_user(data: &[u8]) -> Box<Context> {
        // Parse elf
        let elf = ElfFile::new(data).expect("failed to read elf");
        let is32 = match elf.header.pt2 {
            header::HeaderPt2::Header32(_) => true,
            header::HeaderPt2::Header64(_) => false,
        };
        assert_eq!(elf.header.pt2.type_().as_type(), header::Type::Executable, "ELF is not executable");

        // User stack
        use consts::{USER_STACK_OFFSET, USER_STACK_SIZE, USER32_STACK_OFFSET};
        let (user_stack_buttom, user_stack_top) = match is32 {
            true => (USER32_STACK_OFFSET, USER32_STACK_OFFSET + USER_STACK_SIZE),
            false => (USER_STACK_OFFSET, USER_STACK_OFFSET + USER_STACK_SIZE),
        };

        // Make page table
        let mut memory_set = memory_set_from(&elf);

        // add the new memory set to the recorder
        let mmset_ptr = ((&mut memory_set) as * mut MemorySet) as usize;
        memory_set_record().push_back(mmset_ptr);
        //let id = memory_set_record().iter()
        //    .position(|x| unsafe { info!("current memory set record include {:x?}, {:x?}", x, (*(x.clone() as *mut MemorySet)).get_page_table_mut().token()); false });

        memory_set.push(MemoryArea::new(user_stack_buttom, user_stack_top, MemoryAttr::default().user(), "user_stack"));
        trace!("{:#x?}", memory_set);

        let entry_addr = elf.header.pt2.entry_point() as usize;

        // Temporary switch to it, in order to copy data
        unsafe {
            memory_set.with(|| {
                for ph in elf.program_iter() {
                    let virt_addr = ph.virtual_addr() as usize;
                    let offset = ph.offset() as usize;
                    let file_size = ph.file_size() as usize;
                    if file_size == 0 {
                        return;
                    }
                    use core::slice;
                    let target = unsafe { slice::from_raw_parts_mut(virt_addr as *mut u8, file_size) };
                    target.copy_from_slice(&data[offset..offset + file_size]);
                }
                if is32 {
                    unsafe {
                        // TODO: full argc & argv
                        *(user_stack_top as *mut u32).offset(-1) = 0; // argv
                        *(user_stack_top as *mut u32).offset(-2) = 0; // argc
                    }
                }
            });
        }

        let kstack = KernelStack::new();

        // map the memory set swappable
        //memory_set_map_swappable(&mut memory_set);
        
        //set the user Memory pages in the memory set swappable
        //memory_set_map_swappable(&mut memory_set);
        let id = memory_set_record().iter()
            .position(|x| x.clone() == mmset_ptr).unwrap();
        memory_set_record().remove(id);

        Box::new(ContextImpl {
            arch: unsafe {
                ArchContext::new_user_thread(
                    entry_addr, user_stack_top - 8, kstack.top(), is32, memory_set.token())
            },
            memory_set,
            kstack,
        })
    }

    /// Fork
    pub fn fork(&self, tf: &TrapFrame) -> Box<Context> {
        // Clone memory set, make a new page table
        let mut memory_set = self.memory_set.clone();
        
        // add the new memory set to the recorder
        debug!("fork! new page table token: {:x?}", memory_set.token());
        let mmset_ptr = ((&mut memory_set) as * mut MemorySet) as usize;
        memory_set_record().push_back(mmset_ptr);
        
        // Copy data to temp space
        use alloc::vec::Vec;
        let datas: Vec<Vec<u8>> = memory_set.iter().map(|area| {
            Vec::from(unsafe { area.as_slice() })
        }).collect();

        // Temporary switch to it, in order to copy data
        unsafe {
            memory_set.with(|| {
                for (area, data) in memory_set.iter().zip(datas.iter()) {
                    unsafe { area.as_slice_mut() }.copy_from_slice(data.as_slice())
                }
            });
        }

        let kstack = KernelStack::new();

        // map the memory set swappable
        //memory_set_map_swappable(&mut memory_set);
        // remove the raw pointer for the memory set since it will 
        let id = memory_set_record().iter()
            .position(|x| x.clone() == mmset_ptr).unwrap();
        memory_set_record().remove(id);
        
        Box::new(ContextImpl {
            arch: unsafe { ArchContext::new_fork(tf, kstack.top(), memory_set.token()) },
            memory_set,
            kstack,
        })
    }

    pub fn get_memory_set_mut(&mut self) -> &mut MemorySet {
        &mut self.memory_set
    }

}

impl Drop for ContextImpl{
    fn drop(&mut self){
        // remove the new memory set to the recorder (deprecated in the latest version)
        /*
        let id = memory_set_record().iter()
            .position(|x| unsafe{(*(x.clone() as *mut MemorySet)).token() == self.memory_set.token()});
        if id.is_some(){
            info!("remove id {:x?}", id.unwrap());
            memory_set_record().remove(id.unwrap());
        }
        */
        
        //set the user Memory pages in the memory set unswappable
        let Self {ref mut arch, ref mut memory_set, ref mut kstack} = self;
        let pt = {
            memory_set.get_page_table_mut() as *mut InactivePageTable0
        };
        for area in memory_set.iter(){
            for page in Page::range_of(area.get_start_addr(), area.get_end_addr()) {
                let addr = page.start_address();
                unsafe {
                    active_table_swap().remove_from_swappable(pt, addr, || alloc_frame().unwrap());
                }
            }
        }
        debug!("Finishing setting pages unswappable");
        
    }
}

impl Debug for ContextImpl {
    fn fmt(&self, f: &mut Formatter) -> Result<(), Error> {
        write!(f, "{:x?}", self.arch)
    }
}

/*
* @param: 
*   elf: the source ELF file
* @brief: 
*   generate a memory set according to the elf file
* @retval: 
*   the new memory set
*/
fn memory_set_from<'a>(elf: &'a ElfFile<'a>) -> MemorySet {
    debug!("come in to memory_set_from");
    let mut set = MemorySet::new();
    for ph in elf.program_iter() {
        if ph.get_type() != Ok(Type::Load) {
            continue;
        }
        let (virt_addr, mem_size, flags) = match ph {
            ProgramHeader::Ph32(ph) => (ph.virtual_addr as usize, ph.mem_size as usize, ph.flags),
            ProgramHeader::Ph64(ph) => (ph.virtual_addr as usize, ph.mem_size as usize, ph.flags),
        };
        set.push(MemoryArea::new(virt_addr, virt_addr + mem_size, memory_attr_from(flags), ""));

    }
    set
}

fn memory_attr_from(elf_flags: Flags) -> MemoryAttr {
    let mut flags = MemoryAttr::default().user();
    // TODO: handle readonly
    if elf_flags.is_execute() { flags = flags.execute(); }
    flags
}

/*
* @param: 
*   memory_set: the target MemorySet to set swappable
* @brief:
*   map the memory area in the memory_set swappalbe, specially for the user process
*/
pub fn memory_set_map_swappable(memory_set: &mut MemorySet){
    let pt = unsafe {
        memory_set.get_page_table_mut() as *mut InactivePageTable0
    };
    for area in memory_set.iter(){
        for page in Page::range_of(area.get_start_addr(), area.get_end_addr()) {
            let addr = page.start_address();
            unsafe { active_table_swap().set_swappable(pt, addr); }
        }
    }
    info!("Finishing setting pages swappable");
}

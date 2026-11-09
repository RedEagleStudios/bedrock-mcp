//! Redirecting a module's imports of system functions.

use std::ffi::CStr;

/// `IMAGE_DIRECTORY_ENTRY_IMPORT` in a PE32+ optional header.
const IMPORT_DIRECTORY: usize = 112 + 8;
const DESCRIPTOR_SIZE: usize = 20;
const ORDINAL_FLAG: u64 = 1 << 63;

/// # Safety
/// `at` must be readable for four bytes.
unsafe fn u32_at(at: *const u8) -> u32 {
    unsafe { at.cast::<u32>().read_unaligned() }
}

/// The import-address-table slot through which `module` calls `function` from `dll`.
///
/// # Safety
/// `module` must be the base of a PE32+ image mapped in this process.
pub unsafe fn slot(module: *const u8, dll: &str, function: &str) -> Option<*mut usize> {
    unsafe {
        let headers = module.add(u32_at(module.add(0x3C)) as usize);
        let optional = headers.add(24);
        let imports = u32_at(optional.add(IMPORT_DIRECTORY)) as usize;
        if imports == 0 {
            return None;
        }
        let mut descriptor = module.add(imports);
        loop {
            let name = u32_at(descriptor.add(12)) as usize;
            if name == 0 {
                return None;
            }
            let names = u32_at(descriptor) as usize;
            let addresses = u32_at(descriptor.add(16)) as usize;
            let library = CStr::from_ptr(module.add(name).cast());
            if names != 0 && library.to_bytes().eq_ignore_ascii_case(dll.as_bytes()) {
                let mut index = 0;
                loop {
                    let thunk = module.add(names).cast::<u64>().add(index).read();
                    if thunk == 0 {
                        break;
                    }
                    if thunk & ORDINAL_FLAG == 0 {
                        // IMAGE_IMPORT_BY_NAME: a two-byte hint, then the name.
                        let imported = CStr::from_ptr(module.add(thunk as usize + 2).cast());
                        if imported.to_bytes() == function.as_bytes() {
                            return Some(
                                module.add(addresses).cast::<usize>().add(index).cast_mut(),
                            );
                        }
                    }
                    index += 1;
                }
            }
            descriptor = descriptor.add(DESCRIPTOR_SIZE);
        }
    }
}

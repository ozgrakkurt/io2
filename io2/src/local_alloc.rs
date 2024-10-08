use std::{
    alloc::{AllocError, Allocator, Layout},
    cell::RefCell,
    io,
    marker::PhantomData,
    ptr::NonNull,
};

const ONE_GB: usize = 1024 * 1024 * 1024;
const TWO_MB: usize = 2 * 1024 * 1024;

thread_local! {
    static PAGES: RefCell<State> = RefCell::new(State::new());
}

struct State {
    alloc: unsafe fn(size: usize) -> io::Result<NonNull<[u8]>>,
    // TODO: do allocation of these vectors with a good strategy instead of using global allocator
    pages: Vec<Page>,
    free_list: Vec<Vec<FreeRange>>,
}

const HUGE_PAGE_SIZE_ENV_VAR_NAME: &str = "LOCAL_ALLOC_HUGE_PAGE_SIZE";

impl State {
    fn new() -> Self {
        let alloc = match std::env::var(HUGE_PAGE_SIZE_ENV_VAR_NAME) {
            Err(e) => {
                log::trace!("failed to read {} from environment: {}\nDefaulting using regular 2MB aligned allocations", HUGE_PAGE_SIZE_ENV_VAR_NAME, e);
                alloc_2mb
            }
            Ok(huge_page_size) => match huge_page_size.as_str() {
                "2MB" => {
                    log::trace!("using explicit 2MB huge pages");
                    alloc_2mb_explicit
                }
                "1GB" => {
                    log::trace!("using explicit 1GB huge pages");
                    alloc_1gb_explicit
                }
                _ => {
                    log::trace!(
                        "unknown value read from {} in environment: {}. Expected 2MB or 1GB.\nDefaulting using regular 2MB aligned allocations",
                        HUGE_PAGE_SIZE_ENV_VAR_NAME,
                        huge_page_size
                    );
                    alloc_2mb
                }
            },
        };

        Self {
            alloc,
            pages: Vec::with_capacity(128),
            free_list: Vec::with_capacity(128),
        }
    }
}

#[derive(Clone, Copy)]
struct Page {
    ptr: *mut u8,
    size: usize,
}

#[derive(Clone, Copy)]
struct FreeRange {
    start: usize,
    len: usize,
}

#[derive(Clone, Copy)]
pub struct LocalAlloc {
    _non_send: PhantomData<*mut ()>,
}

impl LocalAlloc {
    #[inline(always)]
    pub fn new() -> Self {
        Self {
            _non_send: PhantomData,
        }
    }
}

unsafe impl Allocator for LocalAlloc {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        PAGES.with_borrow_mut(|pages| {
            for page in pages.iter_mut() {
                let mut alloc = None;
                for (page_idx, range) in page.free_list.iter().enumerate() {
                    let alloc_start =
                        (page.ptr as usize + range.start).next_multiple_of(layout.align());
                    let alloc_start = alloc_start - page.ptr as usize;
                    if alloc_start + layout.size() > range.start + range.len {
                        continue;
                    }

                    alloc = Some((page_idx, alloc_start));
                    break;
                }

                if let Some(alloc) = alloc {
                    let mut range = *page.free_list.get(alloc.0).unwrap();
                    if alloc.1 == range.start {
                        range.start += layout.size();
                        range.len -= layout.size();
                    } else {
                        let new_len = alloc.1 - range.start;
                        page.free_list.push(FreeRange {
                            start: alloc.1 + layout.size(),
                            len: range.len - new_len - layout.size(),
                        });
                        range.len = new_len;
                    }
                    page.free_list[alloc.0] = range;

                    unsafe {
                        return Ok(NonNull::new_unchecked(std::ptr::slice_from_raw_parts_mut(
                            page.ptr.add(alloc.1),
                            layout.size(),
                        )));
                    }
                }
            }

            let align = ALIGN.next_multiple_of(layout.align());
            let size = ALIGN.next_multiple_of(layout.size());
            let page_layout = Layout::from_size_align(size, align).unwrap();
            let ptr = unsafe { std::alloc::alloc(page_layout) };
            let mut free_list = Vec::with_capacity(32);
            free_list.push(FreeRange {
                start: layout.size(),
                len: size - layout.size(),
            });

            pages.push(Page {
                ptr,
                layout: page_layout,
                free_list,
            });

            unsafe {
                Ok(NonNull::new_unchecked(std::ptr::slice_from_raw_parts_mut(
                    ptr,
                    layout.size(),
                )))
            }
        })
    }

    unsafe fn deallocate(&self, ptr: std::ptr::NonNull<u8>, layout: Layout) {
        let ptr = ptr.as_ptr();
        PAGES.with_borrow_mut(|pages| {
            for page in pages.iter_mut() {
                if page.ptr <= ptr && page.ptr.add(page.layout.size()) >= ptr.add(layout.size()) {
                    let start = ptr.sub(page.ptr as usize) as usize;
                    let end = start + layout.size();
                    let mut found = false;
                    dbg!((start, end, page.layout.size()));
                    for range in page.free_list.iter_mut() {
                        if start == range.start + range.len {
                            range.len += layout.size();
                            found = true;
                        } else if end == range.start {
                            range.start -= layout.size();
                            found = true;
                        }
                    }
                    if !found {
                        page.free_list.push(FreeRange {
                            start: start,
                            len: layout.size(),
                        });
                    }

                    return;
                }
            }

            panic!("bad deallocate");
        })
    }
}

unsafe fn alloc_2mb(size: usize) -> io::Result<NonNull<[u8]>> {
    let size = size.next_multiple_of(TWO_MB);
    alloc(size, 0)
}

unsafe fn alloc_2mb_explicit(size: usize) -> io::Result<NonNull<[u8]>> {
    let size = size.next_multiple_of(TWO_MB);
    alloc(size, libc::MAP_HUGE_2MB)
}

unsafe fn alloc_1gb_explicit(size: usize) -> io::Result<NonNull<[u8]>> {
    let size = size.next_multiple_of(ONE_GB);
    alloc(size, libc::MAP_HUGE_1GB)
}

unsafe fn alloc(len: usize, huge_page_flag: libc::c_int) -> io::Result<NonNull<[u8]>> {
    match libc::mmap(
        std::ptr::null_mut(),
        len,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | huge_page_flag,
        -1,
        0,
    ) {
        libc::MAP_FAILED => {
            let errno = *libc::__errno_location();
            let err = std::io::Error::from_raw_os_error(errno);
            Err(io::Error::new(
                io::ErrorKind::Other,
                format!("mmap returned error: {}", err),
            ))
        }
        ptr => match NonNull::new(ptr as *mut u8) {
            Some(ptr) => Ok(NonNull::slice_from_raw_parts(ptr, len)),
            None => Err(io::Error::new(
                io::ErrorKind::Other,
                "mmap returned null pointer",
            )),
        },
    }
}

unsafe fn free(ptr: *mut u8, length: usize) -> io::Result<()> {
    match libc::munmap(ptr as *mut libc::c_void, length) {
        0 => Ok(()),
        -1 => {
            let errno = *libc::__errno_location();
            let err = std::io::Error::from_raw_os_error(errno);
            Err(io::Error::new(
                io::ErrorKind::Other,
                format!("failed to free memory: {}", err),
            ))
        }
        x => Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "unexpected return value from munmap: {}. Expected 0 or -1",
                x
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
}

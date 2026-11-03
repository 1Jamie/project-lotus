use lotus_supervisor::shm::*;
fn main(){
  let p = SealedMemfdPlane::new("atk", 4096).expect("plane");
  let ro = p.ro_fd;
  let mut st: libc::stat = unsafe{std::mem::zeroed()};
  unsafe{libc::fstat(ro,&mut st)};
  println!("mode={:o}", st.st_mode&0o7777);
  let path = std::ffi::CString::new(format!("/proc/self/fd/{}",ro)).unwrap();
  let a = unsafe{libc::open(path.as_ptr(), libc::O_RDWR)};
  println!("direct reopen RDWR -> {} errno={}", a, std::io::Error::last_os_error());
  let r = unsafe{libc::fchmod(ro,0o600)};
  println!("fchmod via RO fd -> {}", r);
  let b = unsafe{libc::open(path.as_ptr(), libc::O_RDWR)};
  println!("reopen after fchmod -> {}", b);
  if b>=0 { let m=unsafe{libc::mmap(std::ptr::null_mut(),4096,libc::PROT_READ|libc::PROT_WRITE,libc::MAP_SHARED,b,0)};
    println!("writable mmap ok={}", m!=libc::MAP_FAILED);
    if m!=libc::MAP_FAILED { unsafe{*(m as *mut u8)=0x41}; 
      let ro_m=unsafe{libc::mmap(std::ptr::null_mut(),4096,libc::PROT_READ,libc::MAP_SHARED,ro,0)};
      println!("producer-visible byte via RO map = {:#x}", unsafe{*(ro_m as *const u8)}); } }
}

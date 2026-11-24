#![no_std]
use core::ffi::{c_char,c_void};
use core::panic::PanicInfo;
#[panic_handler]
fn panic(_: &PanicInfo)->! { loop { core::hint::spin_loop() } }
#[link(name="kernel32",kind="raw-dylib",import_name_type="undecorated")]
extern "system" {
 fn GetModuleHandleA(name:*const c_char)->*mut c_void;
 fn GetProcAddress(module:*mut c_void,name:*const c_char)->*mut c_void;
 fn GetCurrentProcessId()->u32;
 fn GetEnvironmentVariableA(name:*const c_char,buf:*mut u8,len:u32)->u32;
}
type PushBoolean=unsafe extern "C" fn(*mut c_void,i32);
unsafe extern "C" fn verified(state:*mut c_void)->i32 {
 let core=GetModuleHandleA(c"LuaCore.dll".as_ptr());
 if core.is_null(){return 0;}
 let push=GetProcAddress(core,c"lua_pushboolean".as_ptr());
 if push.is_null(){return 0;}
 let push:PushBoolean=core::mem::transmute(push);
 let mut value=[0u8;11];
 let len=GetEnvironmentVariableA(c"CRYSTAL_FFXI_POOL_VERIFIED_V1".as_ptr(),value.as_mut_ptr(),11);
 let mut verified=false;
 if len>0 && len<11 {
  let mut pid=0u32;
  for &digit in value.iter().take(len as usize) {
   if !digit.is_ascii_digit(){break;}
   let Some(next)=pid.checked_mul(10).and_then(|n|n.checked_add(u32::from(digit-b'0'))) else {pid=0;break;};
   pid=next;
  }
  verified=value.iter().take(len as usize).all(u8::is_ascii_digit) &&
   pid!=0 && pid==GetCurrentProcessId();
 }
 push(state,verified as i32);
 1
}

#[no_mangle]
pub unsafe extern "C" fn luaopen__CrystalPool(state:*mut c_void)->i32 {
 let core=GetModuleHandleA(c"LuaCore.dll".as_ptr());
 if core.is_null(){return 0;}
 let address=GetProcAddress(core,c"lua_pushcclosure".as_ptr());
 if address.is_null(){return 0;}
 type Closure=unsafe extern "C" fn(*mut c_void,unsafe extern "C" fn(*mut c_void)->i32,i32);
 let push:Closure=core::mem::transmute(address);
 push(state,verified,0);
 1
}

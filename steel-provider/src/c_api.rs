use std::mem;

use crate::WorldgenContext;
use crate::initialize;
use crate::serialize_chunk_sections;

use std::panic::catch_unwind;

#[repr(C)]
pub struct ByteBuffer {
    pub ptr: *mut u8,
    pub len: usize,
    pub cap: usize,
}

#[unsafe(no_mangle)]
pub extern "C" fn steel_provider_init() {
    initialize();
}

#[unsafe(no_mangle)]
pub extern "C" fn steel_provider_worldgen_ctx_new(seed: u64) -> *mut WorldgenContext {
    let ctx_result = catch_unwind(|| WorldgenContext::new(seed));

    match ctx_result {
        Ok(ctx) => opaque_pointer::raw(ctx).expect("Error trying to lend a pointer"),
        Err(error) => {
            eprintln!("{:?}", error);
            panic!("panic!")
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn steel_provider_generate(
    provider: *mut WorldgenContext,
    chunk_x: i32,
    chunk_z: i32,
) -> ByteBuffer {
    let provider = unsafe { opaque_pointer::mut_object(provider) };

    let chunk = provider.unwrap().generate_with_structures(chunk_x, chunk_z);
    let mut vec = serialize_chunk_sections(&chunk);

    let buffer = ByteBuffer {
        ptr: vec.as_mut_ptr(),
        len: vec.len(),
        cap: vec.capacity(),
    };
    mem::forget(vec);

    buffer
}

#[unsafe(no_mangle)]
pub extern "C" fn steel_provider_bytebuf_free(buf: ByteBuffer) {
    if !buf.ptr.is_null() {
        unsafe {
            // Reconstruct the Vec so Rust's drop handler can free the memory
            let _ = Vec::from_raw_parts(buf.ptr, buf.len, buf.cap);
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn steel_provider_worldgen_ctx_free(provider: *mut WorldgenContext) {
    unsafe {
        let _ = opaque_pointer::own_back(provider);
    };
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    pub fn test() {
        steel_provider_init();
        let ptr = steel_provider_worldgen_ctx_new(0);
        let bytes = steel_provider_generate(ptr, 0, 0);
        steel_provider_bytebuf_free(bytes);
        steel_provider_worldgen_ctx_free(ptr);
    }
}

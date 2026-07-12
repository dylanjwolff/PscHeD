type FuzzerTestOneInput =
    unsafe extern "C" fn(data: *const libc::c_uchar, size: usize) -> libc::c_int;

fn schedule_count() -> usize {
    std::env::var("RSCHED_FUZZ_SCHEDULES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5)
}

fn input_hash(data: *const libc::c_uchar, size: usize) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET;
    if !data.is_null() {
        // SAFETY: libFuzzer passes `data` as a valid buffer of `size` bytes
        // for the duration of this callback. A null pointer is handled above.
        for byte in unsafe { std::slice::from_raw_parts(data, size) } {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    }
    hash ^ (size as u64).wrapping_mul(FNV_PRIME)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rsched_fuzzer_test_one_input(
    data: *const libc::c_uchar,
    size: usize,
    test_one_input: FuzzerTestOneInput,
) -> libc::c_int {
    let base_seed = input_hash(data, size);
    let mut result = 0;
    for schedule in 0..schedule_count() {
        crate::rsched_reinit(base_seed.wrapping_add(schedule as u64));
        result = test_one_input(data, size);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_hash_reads_exact_valid_buffer() {
        let data = [0u8, 1, 2, 3, 255];

        let whole = input_hash(data.as_ptr(), data.len());
        let prefix = input_hash(data.as_ptr(), data.len() - 1);
        let empty_at_valid_ptr = input_hash(data.as_ptr(), 0);

        assert_ne!(whole, prefix);
        assert_ne!(whole, empty_at_valid_ptr);
        assert_eq!(whole, input_hash(data.as_ptr(), data.len()));
    }

    #[test]
    fn input_hash_accepts_null_as_empty_data() {
        assert_eq!(input_hash(core::ptr::null(), 0), input_hash([].as_ptr(), 0));
        assert_ne!(
            input_hash(core::ptr::null(), 0),
            input_hash(core::ptr::null(), 16)
        );
    }
}

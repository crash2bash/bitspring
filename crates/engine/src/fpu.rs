//! Flush-to-zero for the audio threads.
//!
//! A filter's tail decays towards zero through denormal (subnormal) numbers, which most CPUs
//! handle many times slower than normal ones. On a quiet passage after the EQ or the FIR, a
//! callback can then miss its deadline and drop out. These values sit hundreds of dB below
//! anything a DAC can play, so treating them as zero changes nothing audible.

/// Turns flush-to-zero on for the current thread and restores the previous setting on drop
/// (the audio callback runs on a thread the OS owns, so it leaves it as it found it).
pub struct FlushDenormals {
    #[allow(dead_code)]
    saved: u64,
}

impl FlushDenormals {
    #[inline(always)]
    pub fn on() -> Self {
        let saved = imp::read();
        imp::write(saved | imp::BITS);
        Self { saved }
    }
}

impl Drop for FlushDenormals {
    #[inline(always)]
    fn drop(&mut self) {
        imp::write(self.saved);
    }
}

#[cfg(target_arch = "x86_64")]
mod imp {
    use std::arch::asm;

    /// MXCSR: FTZ (bit 15) and DAZ (bit 6).
    pub const BITS: u64 = 0x8040;

    #[inline(always)]
    pub fn read() -> u64 {
        let mut v: u32 = 0;
        // SAFETY: stores MXCSR to a local.
        unsafe { asm!("stmxcsr [{}]", in(reg) &mut v, options(nostack, preserves_flags)) };
        v as u64
    }

    #[inline(always)]
    pub fn write(v: u64) {
        let v = v as u32;
        // SAFETY: loads MXCSR from a local; only the FTZ/DAZ bits ever differ from what was read.
        unsafe { asm!("ldmxcsr [{}]", in(reg) &v, options(nostack, preserves_flags, readonly)) };
    }
}

#[cfg(target_arch = "aarch64")]
mod imp {
    use std::arch::asm;

    /// FPCR: FZ (bit 24), for single and double precision.
    pub const BITS: u64 = 1 << 24;

    #[inline(always)]
    pub fn read() -> u64 {
        let v: u64;
        // SAFETY: reads the FP control register.
        unsafe { asm!("mrs {}, fpcr", out(reg) v, options(nostack, preserves_flags)) };
        v
    }

    #[inline(always)]
    pub fn write(v: u64) {
        // SAFETY: writes the FP control register; only the FZ bit ever differs from what was read.
        unsafe { asm!("msr fpcr, {}", in(reg) v, options(nostack, preserves_flags)) };
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
mod imp {
    pub const BITS: u64 = 0;
    pub fn read() -> u64 {
        0
    }
    pub fn write(_: u64) {}
}

#[cfg(all(test, any(target_arch = "x86_64", target_arch = "aarch64")))]
mod tests {
    use super::*;
    use std::hint::black_box;

    #[test]
    fn denormals_become_zero_inside_and_come_back_after() {
        let tiny = black_box(f64::MIN_POSITIVE);
        assert!(black_box(tiny * 0.5) > 0.0, "denormals work normally");
        {
            let _g = FlushDenormals::on();
            assert_eq!(black_box(tiny) * black_box(0.5), 0.0);
            assert_eq!(black_box(f32::MIN_POSITIVE) * black_box(0.5f32), 0.0);
        }
        assert!(black_box(tiny) * black_box(0.5) > 0.0, "setting restored");
    }
}

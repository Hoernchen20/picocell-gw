//! Compiles sx1302_hal (`hal/`, upstream V2.1.0 unmodified) and the platform layer
//! (`shim/`) with arm-none-eabi-gcc, and links newlib-nano and libm from the
//! toolchain's multilib for the RP2350 (Cortex-M33, hard float).

use std::env;
use std::path::PathBuf;
use std::process::Command;

const CPU_FLAGS: &[&str] = &["-mcpu=cortex-m33", "-mthumb", "-mfloat-abi=hard", "-mfpu=fpv5-sp-d16"];

/// Defined both by libc_nano and by Rust's compiler_builtins (see `main`).
const SHADOWED: &[&str] = &[
    "memcpy",
    "memmove",
    "memset",
    "memcmp",
    "bcmp",
    "strlen",
    "__aeabi_memcpy",
    "__aeabi_memcpy4",
    "__aeabi_memcpy8",
    "__aeabi_memmove",
    "__aeabi_memmove4",
    "__aeabi_memmove8",
    "__aeabi_memset",
    "__aeabi_memset4",
    "__aeabi_memset8",
    "__aeabi_memclr",
    "__aeabi_memclr4",
    "__aeabi_memclr8",
];

/// Upstream files in use; loragw_spi.c, loragw_i2c.c, the *_spi.c and USB files are
/// replaced by shim.c, loragw_gps.c and loragw_mcu.c are not needed. loragw_debug.c
/// also wants libtools/tinymt32.c.
const HAL: &[&str] = &[
    "loragw_ad5338r",
    "loragw_aux",
    "loragw_cal",
    "loragw_com",
    "loragw_debug",
    "loragw_hal",
    "loragw_lbt",
    "loragw_reg",
    "loragw_stts751",
    "loragw_sx1250",
    "loragw_sx125x",
    "loragw_sx1261",
    "loragw_sx1302",
    "loragw_sx1302_rx",
    "loragw_sx1302_timestamp",
    "sx1250_com",
    "sx125x_com",
    "sx1261_com",
];

fn gcc_print(arg: &str) -> PathBuf {
    let out = Command::new("arm-none-eabi-gcc")
        .args(CPU_FLAGS)
        .arg("--specs=nano.specs")
        .arg(arg)
        .output()
        .expect("arm-none-eabi-gcc not found");
    PathBuf::from(String::from_utf8(out.stdout).unwrap().trim())
}

fn main() {
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let (hal, shim) = (dir.join("hal"), dir.join("shim"));

    let mut b = cc::Build::new();
    b.compiler("arm-none-eabi-gcc")
        .std("gnu99")
        .flag("--specs=nano.specs")
        .flag("-include")
        .flag(shim.join("pc_compat.h").to_str().unwrap())
        .include(&shim)
        .include(hal.join("inc"))
        .include(hal.join("src"))
        .include(hal.join("libtools"))
        // Upstream builds with -Wall -Wextra and still warns; nothing to act on here.
        .warnings(false);
    for f in CPU_FLAGS {
        b.flag(f);
    }
    for f in HAL {
        b.file(hal.join("src").join(format!("{f}.c")));
    }
    b.file(hal.join("libtools/tinymt32.c"));
    b.file(shim.join("shim.c"));
    b.compile("sx1302");

    // newlib's mem*/str* would replace compiler_builtins' (weak) ones for all Rust code
    // too, and its memcpy reads unaligned words, which fault on device memory such as
    // the USB buffers. A copy of libc_nano keeps them under other names for newlib's own
    // use; the HAL and Rust get compiler_builtins'.
    let libc = gcc_print("-print-file-name=libc_nano.a");
    let libc_dir = libc.parent().expect("newlib-nano not found");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let mut objcopy = Command::new("arm-none-eabi-objcopy");
    for s in SHADOWED {
        objcopy.arg(format!("--redefine-sym={s}=newlib_{s}"));
    }
    let status = objcopy.arg(&libc).arg(out.join("libc_pc.a")).status().expect("arm-none-eabi-objcopy not found");
    assert!(status.success(), "objcopy of libc_nano.a failed");
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-search=native={}", libc_dir.display());
    println!("cargo:rustc-link-lib=static=c_pc");
    println!("cargo:rustc-link-lib=static=m");

    println!("cargo:rerun-if-changed=hal");
    println!("cargo:rerun-if-changed=shim");
}

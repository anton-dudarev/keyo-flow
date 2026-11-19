use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || env::var_os("CARGO_FEATURE_GUI").is_none()
    {
        return;
    }

    println!("cargo:rerun-if-changed=assets/keyo.rc");
    println!("cargo:rerun-if-changed=assets/keyo.ico");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo не задал OUT_DIR"));
    let resource = if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
        let resource = out_dir.join("keyo-icon.o");
        compile_resource(
            Command::new("windres")
                .args(["-i", "keyo.rc", "-O", "coff", "-o"])
                .arg(&resource),
        );
        resource
    } else {
        let resource = out_dir.join("keyo-icon.res");
        compile_resource(
            Command::new(windows_resource_compiler())
                .args(["/nologo", "/fo"])
                .arg(&resource)
                .arg("keyo.rc"),
        );
        resource
    };

    println!("cargo:rustc-link-arg-bin=keyo-gui={}", resource.display());
}

fn compile_resource(command: &mut Command) {
    let status = command
        .current_dir("assets")
        .status()
        .unwrap_or_else(|error| panic!("Не удалось запустить {command:?}: {error}"));
    assert!(
        status.success(),
        "Не удалось собрать ресурс иконки: {status}"
    );
}

fn windows_resource_compiler() -> PathBuf {
    for name in [
        "PATH",
        "WindowsSdkDir",
        "WindowsSDKVersion",
        "ProgramFiles(x86)",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }

    if let Some(path) = env::var_os("PATH") {
        if let Some(compiler) = env::split_paths(&path)
            .map(|dir| dir.join("rc.exe"))
            .find(|file| file.is_file())
        {
            return compiler;
        }
    }

    let sdk = env::var_os("WindowsSdkDir").map(PathBuf::from).or_else(|| {
        env::var_os("ProgramFiles(x86)")
            .map(|dir| PathBuf::from(dir).join("Windows Kits").join("10"))
    });
    let arch = if env::var("HOST").is_ok_and(|host| host.starts_with("i686-")) {
        "x86"
    } else {
        "x64"
    };

    sdk.and_then(|sdk| {
        let bin = sdk.join("bin");
        if let Ok(version) = env::var("WindowsSDKVersion") {
            let compiler = bin.join(version).join(arch).join("rc.exe");
            if compiler.is_file() {
                return Some(compiler);
            }
        }

        bin.read_dir()
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let version = entry
                    .file_name()
                    .to_str()?
                    .split('.')
                    .map(str::parse::<u32>)
                    .collect::<Result<Vec<_>, _>>()
                    .ok()?;
                let compiler = entry.path().join(arch).join("rc.exe");
                compiler.is_file().then_some((version, compiler))
            })
            .max_by(|(left, _), (right, _)| left.cmp(right))
            .map(|(_, compiler)| compiler)
    })
    .expect("Не найден rc.exe. Для сборки Keyo GUI нужен Windows SDK.")
}

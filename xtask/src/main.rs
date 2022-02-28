use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;

mod symbols;

const USAGE_STRING: &str = "Usage:
  cargo xtask bundle <package> [--release] [--target <triple>]
  cargo xtask bundle -p <package1> -p <package2> ... [--release] [--target <triple>]";

/// The base birectory for the bundler's output.
const BUNDLE_HOME: &str = "target/bundled";

/// Any additional configuration that might be useful for creating plugin bundles, stored as
/// `bundler.toml` alongside the workspace's main `Cargo.toml` file.
type BundlerConfig = HashMap<String, PackageConfig>;

#[derive(Debug, Clone, Deserialize)]
struct PackageConfig {
    name: Option<String>,
}

/// The target we're generating a plugin for. This can be either the native target or a cross
/// compilation target, so to reduce redundancy when determining the correct bundle paths we'll use
/// an enum for this.
///
/// TODO: Right now we don't consider ARM targets at all
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompilationTarget {
    Linux64,
    Linux32,
    Mac64,
    Windows64,
    Windows32,
}

fn main() -> Result<()> {
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("Could not find project root")?;
    std::env::set_current_dir(project_root)
        .context("Could not change to project root directory")?;

    let mut args = std::env::args().skip(1);
    let command = args
        .next()
        .context(format!("Missing command name\n\n{USAGE_STRING}"))?;
    match command.as_str() {
        "bundle" => {
            // For convenience's sake we'll allow building multiple packages with -p just like carg
            // obuild, but you can also build a single package without specifying -p
            let mut args = args.peekable();
            let mut packages = Vec::new();
            if args.peek().map(|s| s.as_str()) == Some("-p") {
                while args.peek().map(|s| s.as_str()) == Some("-p") {
                    packages.push(
                        args.nth(1)
                            .context(format!("Missing package name after -p\n\n{USAGE_STRING}"))?,
                    );
                }
            } else {
                packages.push(
                    args.next()
                        .context(format!("Missing package name\n\n{USAGE_STRING}"))?,
                );
            };
            let other_args: Vec<_> = args.collect();

            bundle(&packages[0], &other_args)?;
            for package in packages.into_iter().skip(1) {
                eprintln!();
                bundle(&package, &other_args)?;
            }

            Ok(())
        }
        // This is only meant to be used by the CI, since using awk for this can be a bit spotty on
        // macOS
        "known-packages" => list_known_packages(),
        _ => bail!("Unknown command '{command}'\n\n{USAGE_STRING}"),
    }
}

// TODO: The macOS version has not been tested
fn bundle(package: &str, args: &[String]) -> Result<()> {
    let bundle_name = match load_bundler_config()?.and_then(|c| c.get(package).cloned()) {
        Some(PackageConfig { name: Some(name) }) => name,
        _ => package.to_string(),
    };

    let mut is_release_build = false;
    let mut cross_compile_target: Option<String> = None;
    for arg_idx in (0..args.len()).rev() {
        let arg = &args[arg_idx];
        match arg.as_str() {
            "--release" => is_release_build = true,
            "--target" => {
                // When cross compiling we should generate the correct bundle type
                cross_compile_target = Some(
                    args.get(arg_idx + 1)
                        .context("Missing cross-compile target")?
                        .to_owned(),
                );
            }
            arg if arg.starts_with("--target=") => {
                cross_compile_target = Some(
                    arg.strip_prefix("--target=")
                        .context("Missing cross-compile target")?
                        .to_owned(),
                );
            }
            _ => (),
        }
    }

    let status = Command::new("cargo")
        .arg("build")
        .arg("-p")
        .arg(package)
        .args(args)
        .status()
        .context(format!("Could not call cargo to build {package}"))?;
    if !status.success() {
        bail!("Could not build {}", package);
    }

    let compilation_target = compilation_target(cross_compile_target.as_deref())?;
    let lib_path = Path::new(target_base(cross_compile_target.as_deref())?)
        .join(if is_release_build { "release" } else { "debug" })
        .join(library_basename(package, compilation_target));
    if !lib_path.exists() {
        bail!("Could not find built library at '{}'", lib_path.display());
    }

    // We'll detect the pugin formats supported by the plugin binary and create bundled accordingly
    // TODO: Support VST2 and CLAP here
    let bundle_vst3 = symbols::exported(&lib_path, "GetPluginFactory")
        .with_context(|| format!("Could not parse '{}'", lib_path.display()))?;

    eprintln!();
    if bundle_vst3 {
        let vst3_lib_path =
            Path::new(BUNDLE_HOME).join(vst3_bundle_library_name(&bundle_name, compilation_target));
        let vst3_bundle_home = vst3_lib_path
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap();

        fs::create_dir_all(vst3_lib_path.parent().unwrap())
            .context("Could not create bundle directory")?;
        reflink::reflink_or_copy(&lib_path, &vst3_lib_path)
            .context("Could not copy library to bundle")?;

        maybe_create_macos_vst3_bundle(package, compilation_target)?;

        eprintln!("Created a VST3 bundle at '{}'", vst3_bundle_home.display());
    } else {
        eprintln!("Not creating any plugin bundles because the package does not export any plugins")
    }

    Ok(())
}

/// This lists the packages configured in `bundler.toml`. This is only used as part of the CI when
/// bundling plugins.
fn list_known_packages() -> Result<()> {
    if let Some(config) = load_bundler_config()? {
        for package in config.keys() {
            println!("{package}");
        }
    }

    Ok(())
}

/// Load the `bundler.toml` file, if it exists. If it does exist but it cannot be parsed, then this
/// will return an error.
fn load_bundler_config() -> Result<Option<BundlerConfig>> {
    // We're already in the project root
    let bundler_config_path = Path::new("bundler.toml");
    if !bundler_config_path.exists() {
        return Ok(None);
    }

    let result = toml::from_str(
        &fs::read_to_string(&bundler_config_path)
            .with_context(|| format!("Could not read '{}'", bundler_config_path.display()))?,
    )
    .with_context(|| format!("Could not parse '{}'", bundler_config_path.display()))?;

    Ok(Some(result))
}

/// The target we're compiling for. This is used to determine the paths and options for creating
/// plugin bundles.
fn compilation_target(cross_compile_target: Option<&str>) -> Result<CompilationTarget> {
    match cross_compile_target {
        Some("x86_64-unknown-linux-gnu") => Ok(CompilationTarget::Linux64),
        Some("x86_64-apple-darwin") => Ok(CompilationTarget::Mac64),
        Some("x86_64-pc-windows-gnu") => Ok(CompilationTarget::Windows64),
        Some(target) => bail!("Unhandled cross-compilation target: {}", target),
        None => {
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            return Ok(CompilationTarget::Linux64);
            #[cfg(all(target_os = "linux", target_arch = "x86"))]
            return Ok(CompilationTarget::Linux32);
            #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
            return Ok(CompilationTarget::Mac64);
            #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
            return Ok(CompilationTarget::Windows64);
            #[cfg(all(target_os = "windows", target_arch = "x86"))]
            return Ok(CompilationTarget::Windows32);
        }
    }
}

/// The base directory for the compiled binaries. This does not use [CompilationTarget] as we need
/// to be able to differentiate between native and cross-compilation.
fn target_base(cross_compile_target: Option<&str>) -> Result<&'static str> {
    match cross_compile_target {
        Some("x86_64-unknown-linux-gnu") => Ok("target/x86_64-unknown-linux-gnu"),
        Some("x86_64-pc-windows-gnu") => Ok("target/x86_64-pc-windows-gnu"),
        Some("x86_64-apple-darwin") => Ok("target/x86_64-apple-darwin"),
        Some(target) => bail!("Unhandled cross-compilation target: {}", target),
        None => Ok("target"),
    }
}

/// The file name of the compiled library for a `cdylib` crate.
fn library_basename(package: &str, target: CompilationTarget) -> String {
    match target {
        CompilationTarget::Linux64 | CompilationTarget::Linux32 => format!("lib{package}.so"),
        CompilationTarget::Mac64 => format!("lib{package}.dylib"),
        CompilationTarget::Windows64 | CompilationTarget::Windows32 => format!("{package}.dll"),
    }
}

// See https://developer.steinberg.help/display/VST/Plug-in+Format+Structure

/// The full path to the library file inside of a VST3 bundle, including the leading `.vst3`
/// directory.
fn vst3_bundle_library_name(package: &str, target: CompilationTarget) -> String {
    match target {
        CompilationTarget::Linux64 => format!("{package}.vst3/Contents/x86_64-linux/{package}.so"),
        CompilationTarget::Linux32 => format!("{package}.vst3/Contents/i386-linux/{package}.so"),
        CompilationTarget::Mac64 => format!("{package}.vst3/Contents/MacOS/{package}"),
        CompilationTarget::Windows64 => {
            format!("{package}.vst3/Contents/x86_64-win/{package}.vst3")
        }
        CompilationTarget::Windows32 => format!("{package}.vst3/Contents/x86-win/{package}.vst3"),
    }
}

/// If compiling for macOS, create all of the bundl-y stuff Steinberg and Apple require you to have.
fn maybe_create_macos_vst3_bundle(package: &str, target: CompilationTarget) -> Result<()> {
    if target != CompilationTarget::Mac64 {
        return Ok(());
    }

    // TODO: May want to add bundler.toml fields for the identifier, version and signature at some
    //       point.
    fs::write(
        format!("{}/{}.vst3/Contents/PkgInfo", BUNDLE_HOME, package),
        "BNDL????",
    )
    .context("Could not create PkgInfo file")?;
    fs::write(
        format!("{}/{}.vst3/Contents/Info.plist", BUNDLE_HOME, package),
        format!(r#"<?xml version="1.0" encoding="UTF-8"?>

<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist>
  <dict>
    <key>CFBundleExecutable</key>
    <string>{package}</string>
    <key>CFBundleIconFile</key>
    <string></string>
    <key>CFBundleIdentifier</key>
    <string>com.nih-plug.{package}</string>
    <key>CFBundleName</key>
    <string>{package}</string>
    <key>CFBundleDisplayName</key>
    <string>{package}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleSignature</key>
    <string>????</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0.0</string>
    <key>CFBundleVersion</key>
    <string>1.0.0</string>
    <key>NSHumanReadableCopyright</key>
    <string></string>
    <key>NSHighResolutionCapable</key>
    <true/>
  </dict>
</plist>
"#),
    )
    .context("Could not create Info.plist file")?;

    Ok(())
}

use std::collections::HashMap;
use std::env;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

fn get_features_code(category: &str) -> &'static str {
    match category {
        "amp" | "distortion" => r#"&[c"audio-effect", c"distortion", c"stereo"]"#,
        "cabinet" | "filter" => r#"&[c"audio-effect", c"filter", c"stereo"]"#,
        "delay" => r#"&[c"audio-effect", c"delay", c"stereo"]"#,
        "reverb" => r#"&[c"audio-effect", c"reverb", c"stereo"]"#,
        "modulation" => r#"&[c"audio-effect", c"chorus", c"stereo"]"#,
        "pitch" => r#"&[c"audio-effect", c"pitch-shifter", c"stereo"]"#,
        "dynamics" => r#"&[c"audio-effect", c"compressor", c"stereo"]"#,
        "mic" | "utility" => r#"&[c"audio-effect", c"utility", c"stereo"]"#,
        _ => r#"&[c"audio-effect", c"stereo"]"#,
    }
}

fn collect_assets(dir: &Path, prefix: &str, assets: &mut Vec<(String, String)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            let Some(rel) = path.strip_prefix(dir).ok() else {
                continue;
            };
            let rel_path = rel.to_string_lossy().to_string();
            let key = if prefix.is_empty() {
                rel_path
            } else {
                format!("{prefix}/{rel_path}")
            };
            assets.push((key, path.to_string_lossy().to_string()));
        } else if path.is_dir() {
            let sub_name = path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().to_string());
            let new_prefix = if prefix.is_empty() {
                sub_name
            } else {
                format!("{prefix}/{sub_name}")
            };
            collect_assets(&path, &new_prefix, assets);
        }
    }
}

fn find_case_insensitive_dir(parent: &Path, target: &str) -> Option<PathBuf> {
    if let Ok(entries) = fs::read_dir(parent) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                if let Some(name) = p.file_name() {
                    if name.to_string_lossy().eq_ignore_ascii_case(target) {
                        return Some(p);
                    }
                }
            }
        }
    }
    None
}

fn resolve_dll_path(manifest_dir: &Path, plugin_name: &str) -> String {
    env::var("ZFX_DLL_PATH").unwrap_or_else(|_| {
        manifest_dir
            .parent()
            .map(|p| {
                let candidate = p
                    .join("wine_prefix/drive_c/Program Files (x86)/ZOOM/ZFX/resources")
                    .join(format!("{plugin_name}.dll"));
                if candidate.exists() {
                    candidate.to_string_lossy().to_string()
                } else {
                    String::new()
                }
            })
            .unwrap_or_default()
    })
}

fn resolve_worker_path(manifest_dir: &Path) -> String {
    env::var("ZFX_WORKER_PATH").unwrap_or_else(|_| {
        manifest_dir
            .parent()
            .map(|p| {
                let candidate = p.join("zfx-worker32/target/release/zfx-worker32");
                if candidate.exists() {
                    candidate.to_string_lossy().to_string()
                } else {
                    String::new()
                }
            })
            .unwrap_or_default()
    })
}

fn resolve_xml_path(manifest_dir: &Path, plugin_name: &str) -> String {
    env::var("ZFX_XML_PATH").unwrap_or_else(|_| {
        let default_xml = manifest_dir
            .join("assets")
            .join(plugin_name)
            .join("EfxType.xml");
        if default_xml.exists() {
            default_xml.to_string_lossy().to_string()
        } else {
            let fallback_xml = manifest_dir
                .join("assets")
                .join("100BassMan")
                .join("EfxType.xml");
            if fallback_xml.exists() {
                fallback_xml.to_string_lossy().to_string()
            } else {
                String::new()
            }
        }
    })
}

fn build_asset_map(manifest_dir: &Path, plugin_name: &str) -> HashMap<String, String> {
    let mut asset_map = HashMap::new();
    let mut common_assets = Vec::new();
    for sub in &["AmpMdl", "Bkgd", "EffMdl", "outMaster", "outTuner"] {
        let sub_dir = manifest_dir.join("assets").join(sub);
        if sub_dir.exists() {
            collect_assets(&sub_dir, sub, &mut common_assets);
            collect_assets(&sub_dir, "", &mut common_assets);
        }
    }
    for (k, v) in common_assets {
        let lower = k.to_lowercase();
        asset_map.insert(k.clone(), v.clone());
        if lower != k {
            asset_map.insert(lower.clone(), v.clone());
        }
        if let Some(stem) = k.strip_suffix(".png").or_else(|| k.strip_suffix(".jpg")) {
            asset_map.insert(stem.to_string(), v.clone());
        }
        if let Some(stem) = lower
            .strip_suffix(".png")
            .or_else(|| lower.strip_suffix(".jpg"))
        {
            asset_map.insert(stem.to_string(), v.clone());
        }
    }

    let assets_root = manifest_dir.join("assets");
    let plugin_dir = {
        let direct = assets_root.join(plugin_name);
        if direct.exists() {
            Some(direct)
        } else {
            find_case_insensitive_dir(&assets_root, plugin_name)
        }
    };

    if let Some(pdir) = plugin_dir {
        let pdir_name = pdir
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().to_string());
        let mut plugin_assets = Vec::new();
        collect_assets(&pdir, &pdir_name, &mut plugin_assets);
        collect_assets(&pdir, plugin_name, &mut plugin_assets);
        collect_assets(&pdir, "", &mut plugin_assets);
        for (k, v) in plugin_assets {
            let lower = k.to_lowercase();
            asset_map.insert(k.clone(), v.clone());
            if lower != k {
                asset_map.insert(lower.clone(), v.clone());
            }
            if let Some(stem) = k.strip_suffix(".png").or_else(|| k.strip_suffix(".jpg")) {
                asset_map.insert(stem.to_string(), v.clone());
            }
            if let Some(stem) = lower
                .strip_suffix(".png")
                .or_else(|| lower.strip_suffix(".jpg"))
            {
                asset_map.insert(stem.to_string(), v.clone());
            }
        }
    }
    asset_map
}

fn generate_asset_code(manifest_dir: &Path, plugin_name: &str) -> String {
    let asset_map = build_asset_map(manifest_dir, plugin_name);
    let mut path_to_keys: HashMap<String, Vec<String>> = HashMap::new();
    for (key, path) in asset_map {
        if let Ok(abs_path) = fs::canonicalize(&path) {
            let abs_path_str = abs_path.to_string_lossy().to_string();
            path_to_keys.entry(abs_path_str).or_default().push(key);
        }
    }
    let mut arms = String::new();
    let mut sorted_paths: Vec<_> = path_to_keys.into_iter().collect();
    sorted_paths.sort_by(|a, b| a.0.cmp(&b.0));
    for (abs_path_str, mut keys) in sorted_paths {
        keys.sort();
        let safe_path = abs_path_str.replace('\\', "\\\\");
        let pattern = keys
            .iter()
            .map(|k| format!("\"{k}\""))
            .collect::<Vec<_>>()
            .join(" | ");
        let _ = writeln!(
            arms,
            "        {pattern} => Some(include_bytes!(r\"{safe_path}\")),"
        );
    }
    format!(
        "#[must_use]\n#[allow(clippy::all, clippy::match_same_arms, clippy::too_many_lines, clippy::must_use_candidate)]\npub fn get_asset(path: &str) -> Option<&'static [u8]> {{\n    #[allow(clippy::all, clippy::match_same_arms)]\n    match path {{\n{arms}        _ => None,\n    }}\n}}"
    )
}

fn detect_background_dimensions(manifest_dir: &Path, plugin_name: &str) -> (String, String) {
    let asset_dir = manifest_dir.join("assets").join(plugin_name);
    let path_lowercase = asset_dir.join("bkgd.png");
    let path_capitalized = asset_dir.join("Bkgd.png");
    let bkgd_path = if path_lowercase.exists() {
        Some(path_lowercase)
    } else if path_capitalized.exists() {
        Some(path_capitalized)
    } else {
        None
    };

    let Some(p) = bkgd_path else {
        return ("None".to_string(), "None".to_string());
    };
    let Ok(data) = fs::read(&p) else {
        return ("None".to_string(), "None".to_string());
    };
    if data.len() >= 24 && &data[0..4] == b"\x89PNG" {
        let w = f64::from(u32::from_be_bytes(data[16..20].try_into().unwrap()));
        let h = f64::from(u32::from_be_bytes(data[20..24].try_into().unwrap()));
        (format!("Some({w:.1})"), format!("Some({h:.1})"))
    } else {
        ("None".to_string(), "None".to_string())
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=ZFX_DLL_PATH");
    println!("cargo:rerun-if-env-changed=ZFX_PLUGIN_ID");
    println!("cargo:rerun-if-env-changed=ZFX_PLUGIN_NAME");
    println!("cargo:rerun-if-env-changed=ZFX_PLUGIN_CATEGORY");
    println!("cargo:rerun-if-env-changed=ZFX_XML_PATH");
    println!("cargo:rerun-if-env-changed=ZFX_WORKER_PATH");
    println!("cargo:rerun-if-env-changed=LD_LIBRARY_PATH");

    if let Ok(ld_lib_path) = env::var("LD_LIBRARY_PATH") {
        for path in ld_lib_path.split(':') {
            if !path.is_empty() && Path::new(path).exists() {
                println!("cargo:rustc-link-arg=-Wl,-rpath,{path}");
            }
        }
    }

    let out_dir = env::var("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("zfx_plugin_config.rs");
    let manifest_dir = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).to_path_buf();

    let plugin_name = env::var("ZFX_PLUGIN_NAME").unwrap_or_else(|_| "100BassMan".to_string());
    let plugin_id = env::var("ZFX_PLUGIN_ID")
        .unwrap_or_else(|_| format!("com.idf.zfx.{}", plugin_name.to_lowercase()));
    let category = env::var("ZFX_PLUGIN_CATEGORY").unwrap_or_else(|_| "effect".to_string());

    let dll_path = resolve_dll_path(&manifest_dir, &plugin_name);
    let worker_path = resolve_worker_path(&manifest_dir);
    if !worker_path.is_empty() {
        println!("cargo:rerun-if-changed={worker_path}");
    }
    let xml_path = resolve_xml_path(&manifest_dir, &plugin_name);

    let payload_code = if !dll_path.is_empty() && Path::new(&dll_path).exists() {
        format!("pub static EMBEDDED_DLL_PAYLOAD: &[u8] = include_bytes!(r\"{dll_path}\");")
    } else {
        "pub static EMBEDDED_DLL_PAYLOAD: &[u8] = &[];".to_string()
    };

    let worker_code = if !worker_path.is_empty() && Path::new(&worker_path).exists() {
        format!("pub static EMBEDDED_WORKER_PAYLOAD: &[u8] = include_bytes!(r\"{worker_path}\");")
    } else {
        "pub static EMBEDDED_WORKER_PAYLOAD: &[u8] = &[];".to_string()
    };

    let xml_code = if !xml_path.is_empty() && Path::new(&xml_path).exists() {
        format!("pub const XML_LAYOUT: &str = include_str!(r\"{xml_path}\");")
    } else {
        "pub const XML_LAYOUT: &str = \"\";".to_string()
    };

    let features_code = get_features_code(&category);
    let asset_code = generate_asset_code(&manifest_dir, &plugin_name);
    let (background_width_code, background_height_code) =
        detect_background_dimensions(&manifest_dir, &plugin_name);

    let content = format!(
        "{payload_code}\n{worker_code}\n{xml_code}\n{asset_code}\npub const PLUGIN_ID: &str = \"{plugin_id}\";\npub const PLUGIN_NAME: &str = \"{plugin_name}\";\npub const PLUGIN_CATEGORY: &str = \"{category}\";\npub const PLUGIN_FEATURES: &[&core::ffi::CStr] = {features_code};\npub const NATIVE_BKGD_WIDTH: Option<f64> = {background_width_code};\npub const NATIVE_BKGD_HEIGHT: Option<f64> = {background_height_code};\n"
    );

    fs::write(dest_path, content).unwrap();
}

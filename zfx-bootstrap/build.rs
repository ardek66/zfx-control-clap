use std::env;
use std::fs;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-env-changed=ZFX_DLL_PATH");
    println!("cargo:rerun-if-env-changed=ZFX_PLUGIN_ID");
    println!("cargo:rerun-if-env-changed=ZFX_PLUGIN_NAME");
    println!("cargo:rerun-if-env-changed=ZFX_PLUGIN_CATEGORY");
    println!("cargo:rerun-if-env-changed=ZFX_XML_PATH");
    println!("cargo:rerun-if-env-changed=ZFX_WORKER_PATH");
    println!("cargo:rerun-if-env-changed=LD_LIBRARY_PATH");

    // Ensure X11 and GL libraries from the environment are baked into the rpath of the built
    // cdylib so that dynamic dlopens in x11-dl (libX11-xcb.so.1, libGL.so.1, libXcursor.so.1, etc.)
    // resolve cleanly even inside host DAWs like REAPER without requiring system-wide paths.
    if let Ok(ld_lib_path) = env::var("LD_LIBRARY_PATH") {
        for path in ld_lib_path.split(':') {
            if !path.is_empty() && Path::new(path).exists() {
                println!("cargo:rustc-link-arg=-Wl,-rpath,{}", path);
            }
        }
    }

    let out_dir = env::var("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("zfx_plugin_config.rs");
    let manifest_dir = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).to_path_buf();

    let plugin_name = env::var("ZFX_PLUGIN_NAME").unwrap_or_else(|_| "100BassMan".to_string());
    let plugin_id = env::var("ZFX_PLUGIN_ID").unwrap_or_else(|_| format!("com.idf.zfx.{}", plugin_name.to_lowercase()));
    let category = env::var("ZFX_PLUGIN_CATEGORY").unwrap_or_else(|_| "effect".to_string());
    let dll_path = env::var("ZFX_DLL_PATH").unwrap_or_else(|_| {
        let candidate = manifest_dir.parent().unwrap()
            .join("wine_prefix/drive_c/Program Files (x86)/ZOOM/ZFX/resources")
            .join(format!("{}.dll", plugin_name));
        if candidate.exists() {
            candidate.to_string_lossy().to_string()
        } else {
            String::new()
        }
    });
    let worker_path = env::var("ZFX_WORKER_PATH").unwrap_or_else(|_| {
        let candidate = manifest_dir.parent().unwrap()
            .join("zfx-worker32/target/release/zfx-worker32");
        if candidate.exists() {
            candidate.to_string_lossy().to_string()
        } else {
            String::new()
        }
    });
    if !worker_path.is_empty() {
        println!("cargo:rerun-if-changed={}", worker_path);
    }
    let xml_path = env::var("ZFX_XML_PATH").unwrap_or_else(|_| {
        let default_xml = manifest_dir.join("assets").join(&plugin_name).join("EfxType.xml");
        if default_xml.exists() {
            default_xml.to_string_lossy().to_string()
        } else {
            let fallback_xml = manifest_dir.join("assets").join("100BassMan").join("EfxType.xml");
            if fallback_xml.exists() {
                fallback_xml.to_string_lossy().to_string()
            } else {
                String::new()
            }
        }
    });

    let payload_code = if !dll_path.is_empty() && Path::new(&dll_path).exists() {
        format!("pub static EMBEDDED_DLL_PAYLOAD: &[u8] = include_bytes!(r\"{}\");", dll_path)
    } else {
        "pub static EMBEDDED_DLL_PAYLOAD: &[u8] = &[];".to_string()
    };

    let worker_code = if !worker_path.is_empty() && Path::new(&worker_path).exists() {
        format!("pub static EMBEDDED_WORKER_PAYLOAD: &[u8] = include_bytes!(r\"{}\");", worker_path)
    } else {
        "pub static EMBEDDED_WORKER_PAYLOAD: &[u8] = &[];".to_string()
    };

    let xml_code = if !xml_path.is_empty() && Path::new(&xml_path).exists() {
        format!("pub const XML_LAYOUT: &str = include_str!(r\"{}\");", xml_path)
    } else {
        "pub const XML_LAYOUT: &str = \"\";".to_string()
    };

    let features_code = match category.as_str() {
        "amp" => r#"&[c"audio-effect", c"distortion", c"stereo"]"#,
        "cabinet" => r#"&[c"audio-effect", c"filter", c"stereo"]"#,
        "distortion" => r#"&[c"audio-effect", c"distortion", c"stereo"]"#,
        "delay" => r#"&[c"audio-effect", c"delay", c"stereo"]"#,
        "reverb" => r#"&[c"audio-effect", c"reverb", c"stereo"]"#,
        "modulation" => r#"&[c"audio-effect", c"chorus", c"stereo"]"#,
        "pitch" => r#"&[c"audio-effect", c"pitch-shifter", c"stereo"]"#,
        "filter" => r#"&[c"audio-effect", c"filter", c"stereo"]"#,
        "dynamics" => r#"&[c"audio-effect", c"compressor", c"stereo"]"#,
        "mic" => r#"&[c"audio-effect", c"utility", c"stereo"]"#,
        "utility" => r#"&[c"audio-effect", c"utility", c"stereo"]"#,
        _ => r#"&[c"audio-effect", c"stereo"]"#,
    };

    fn collect_assets(dir: &Path, prefix: &str, assets: &mut Vec<(String, String)>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    let rel_path = path.strip_prefix(dir).unwrap().to_string_lossy().to_string();
                    let key = if prefix.is_empty() {
                        rel_path.clone()
                    } else {
                        format!("{}/{}", prefix, rel_path)
                    };
                    let key = key.replace("\\", "/");
                    assets.push((key, path.to_string_lossy().to_string()));
                } else if path.is_dir() {
                    let dir_name = path.file_name().unwrap().to_string_lossy().to_string();
                    let new_prefix = if prefix.is_empty() {
                        dir_name
                    } else {
                        format!("{}/{}", prefix, dir_name)
                    };
                    collect_assets(&path, &new_prefix, assets);
                }
            }
        }
    }

    let mut asset_map = std::collections::BTreeMap::new();

    let assets_base = manifest_dir.join("assets");

    let common_dir = assets_base.join("Common");
    if common_dir.exists() {
        let mut common_assets = Vec::new();
        collect_assets(&common_dir, "Common", &mut common_assets);
        collect_assets(&common_dir, "", &mut common_assets);
        for (k, v) in common_assets {
            let lower = k.to_lowercase();
            asset_map.insert(k.clone(), v.clone());
            if lower != k {
                asset_map.entry(lower.clone()).or_insert_with(|| v.clone());
            }
            if let Some(stem) = k.strip_suffix(".png").or_else(|| k.strip_suffix(".jpg")) {
                asset_map.entry(stem.to_string()).or_insert_with(|| v.clone());
            }
            if let Some(stem) = lower.strip_suffix(".png").or_else(|| lower.strip_suffix(".jpg")) {
                asset_map.entry(stem.to_string()).or_insert_with(|| v.clone());
            }
        }
    }
    
    let plugin_dir = {
        let direct = assets_base.join(&plugin_name);
        if direct.exists() {
            Some(direct)
        } else {
            let mut found = None;
            if let Ok(entries) = fs::read_dir(&assets_base) {
                for entry in entries.flatten() {
                    if entry.path().is_dir() && entry.file_name().to_string_lossy().eq_ignore_ascii_case(&plugin_name) {
                        found = Some(entry.path());
                        break;
                    }
                }
            }
            found
        }
    };

    if let Some(pdir) = plugin_dir {
        let pdir_name = pdir.file_name().unwrap().to_string_lossy().to_string();
        let mut plugin_assets = Vec::new();
        collect_assets(&pdir, &pdir_name, &mut plugin_assets);
        collect_assets(&pdir, &plugin_name, &mut plugin_assets);
        collect_assets(&pdir, "", &mut plugin_assets);
        for (k, v) in plugin_assets {
            // Plugin assets override common assets
            let lower = k.to_lowercase();
            asset_map.insert(k.clone(), v.clone());
            if lower != k {
                asset_map.insert(lower.clone(), v.clone());
            }
            if let Some(stem) = k.strip_suffix(".png").or_else(|| k.strip_suffix(".jpg")) {
                asset_map.insert(stem.to_string(), v.clone());
            }
            if let Some(stem) = lower.strip_suffix(".png").or_else(|| lower.strip_suffix(".jpg")) {
                asset_map.insert(stem.to_string(), v.clone());
            }
        }
    }

    let mut asset_match_arms = String::new();
    for (key, path) in asset_map {
        if let Ok(abs_path) = fs::canonicalize(&path) {
            let abs_path_str = abs_path.to_string_lossy().to_string();
            let safe_path = abs_path_str.replace("\\", "\\\\");
            asset_match_arms.push_str(&format!("        \"{}\" => Some(include_bytes!(r\"{}\")),\n", key, safe_path));
        }
    }

    let asset_code = format!(
r#"pub fn get_asset(path: &str) -> Option<&'static [u8]> {{
    match path {{
{}        _ => None,
    }}
}}"#, asset_match_arms);

    let (bkgd_w_code, bkgd_h_code) = {
        let asset_dir = manifest_dir.join("assets").join(&plugin_name);
        let bkgd1 = asset_dir.join("bkgd.png");
        let bkgd2 = asset_dir.join("Bkgd.png");
        let bkgd_path = if bkgd1.exists() { Some(bkgd1) } else if bkgd2.exists() { Some(bkgd2) } else { None };
        if let Some(p) = bkgd_path {
            if let Ok(data) = fs::read(&p) {
                if data.len() >= 24 && &data[0..4] == b"\x89PNG" {
                    let w = u32::from_be_bytes(data[16..20].try_into().unwrap()) as f64;
                    let h = u32::from_be_bytes(data[20..24].try_into().unwrap()) as f64;
                    (format!("Some({:.1})", w), format!("Some({:.1})", h))
                } else {
                    ("None".to_string(), "None".to_string())
                }
            } else {
                ("None".to_string(), "None".to_string())
            }
        } else {
            ("None".to_string(), "None".to_string())
        }
    };

    let content = format!(
        "{}\n{}\n{}\n{}\npub const PLUGIN_ID: &str = \"{}\";\npub const PLUGIN_NAME: &str = \"{}\";\npub const PLUGIN_CATEGORY: &str = \"{}\";\npub const PLUGIN_FEATURES: &[&core::ffi::CStr] = {};\npub const NATIVE_BKGD_WIDTH: Option<f64> = {};\npub const NATIVE_BKGD_HEIGHT: Option<f64> = {};\n",
        payload_code, worker_code, xml_code, asset_code, plugin_id, plugin_name, category, features_code, bkgd_w_code, bkgd_h_code
    );

    fs::write(dest_path, content).unwrap();
}

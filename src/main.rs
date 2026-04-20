use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;
use anyhow::{Result, bail, anyhow};
use clap::{Parser, Subcommand, ValueEnum};
use reqwest::header::USER_AGENT;
use serde_json::Value;
use wasmtime::{Engine, Linker, Module, Store, Val, Config, OptLevel};
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi::p1::{self, WasiP1Ctx};

#[derive(Parser)]
#[command(name = "rsxtk", version = "0.4.4")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
#[command(rename_all = "lowercase")]
enum Commands {
    /// 🏃 Run a WASI app or script
    Run { path: PathBuf },
    /// 🧩 Execute functions from a library
    Mod { 
        path: PathBuf, 
        #[arg(allow_hyphen_values = true)]
        args: Vec<String> 
    },
    /// 🚀 Native execution via 'cargo -Z script'
    Csrun { path: PathBuf },
    /// 🔨 Build a universal .wasm or .wat file
    Build { 
        path: PathBuf, 
        target: BuildTarget 
    },
    /// 🔄 Convert between .wasm and .wat
    Convert { input: PathBuf, #[arg(short, long)] output: Option<PathBuf> },
    /// ✨ Init a new script template
    Init { name: String },
    /// 📦 Init a new library module template
    InitMod { name: String },
    /// 📂 Create 'rsc' folder for local files
    Resource { path: Option<PathBuf> },
    /// ⏱️ Benchmark execution speed
    Bench { 
        path: PathBuf, 
        #[arg(short, long, default_value = "5")] iterations: u32 
    },
    /// ➕ Add a dependency (Auto-injects manifest if missing)
    Add { path: PathBuf, crate_name: String, version: Option<String> },
    /// ➖ Remove a dependency
    Remove { path: PathBuf, crate_name: String },
    /// 📦 List script dependencies
    List { path: PathBuf },
    /// ✨ Format Rust code (Preflight check for manifests)
    Fmt { path: PathBuf },
    /// 🧹 Optimize WASM via Walrus
    Optimize { #[arg(short, long)] input: PathBuf, #[arg(short, long)] output: Option<PathBuf> },
    /// 🗑️ Wipe the .tk build cache
    Clean,
    /// 📝 View WASM metadata (wasmtk style)
    Info { path: PathBuf },
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
#[value(rename_all = "lowercase")]
enum BuildTarget { Wasi, Wasm, Wat }

struct MyState { wasi: WasiP1Ctx }

fn get_machine_id() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut s = DefaultHasher::new();
    std::env::consts::OS.hash(&mut s);
    std::env::consts::ARCH.hash(&mut s);
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| 
        std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown_host".to_string())
    ).hash(&mut s);
    format!("{:x}", s.finish())
}

fn create_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.cranelift_opt_level(OptLevel::Speed);
    config.wasm_simd(true);
    config.wasm_bulk_memory(true);
    config.wasm_multi_memory(true);
    Engine::new(&config)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let engine = create_engine()?;

    match cli.command {
        Commands::Run { path } => run_wasm(&engine, &path),
        Commands::Mod { path, args } => run_wasm_mod(&engine, &path, args),
        Commands::Csrun { path } => run_cargo_script(&path),
        Commands::Build { path, target } => {
            match target {
                BuildTarget::Wasi | BuildTarget::Wasm => build_with_virtual_cargo(&engine, &path).map(|_| ()),
                BuildTarget::Wat => {
                    let wasm = build_with_virtual_cargo(&engine, &path)?;
                    convert_wasm_wat(&wasm, None)
                },
            }
        },
        Commands::Convert { input, output } => convert_wasm_wat(&input, output),
        Commands::Init { name } => init_script(&name, false),
        Commands::InitMod { name } => init_script(&name, true),
        Commands::Resource { path } => create_resource_dir(path.as_deref()),
        Commands::Bench { path, iterations } => benchmark_wasm(&engine, &path, iterations),
        Commands::Add { path, crate_name, version } => {
            let ver = version.unwrap_or_else(|| get_latest_version(&crate_name));
            edit_deps(&path, &crate_name, Some(&ver))
        },
        Commands::Remove { path, crate_name } => edit_deps(&path, &crate_name, None),
        Commands::List { path } => list_deps(&path),
        Commands::Fmt { path } => format_script(&path),
        Commands::Optimize { input, output } => optimize_wasm(&input, output),
        Commands::Clean => clean_cache(),
        Commands::Info { path } => print_info(&engine, &path),
    }
}

// --- WASM Loading & Execution ---

fn run_wasm(engine: &Engine, path: &Path) -> Result<()> {
    let abs_path = fs::canonicalize(path)?;
    let module = load_module(engine, &abs_path)?;
    execute_module(engine, module, abs_path.parent().unwrap(), abs_path.file_name().unwrap().to_str().unwrap())
}

fn run_wasm_mod(engine: &Engine, path: &Path, args: Vec<String>) -> Result<()> {
    let abs_path = fs::canonicalize(path)?;
    let module = load_module(engine, &abs_path)?;

    let mut linker: Linker<MyState> = Linker::new(engine);
    p1::add_to_linker_sync(&mut linker, |state| &mut state.wasi)?;

    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stdio().inherit_args();
    builder.preopened_dir(abs_path.parent().unwrap(), ".", wasmtime_wasi::DirPerms::all(), wasmtime_wasi::FilePerms::all())?;
    
    let mut store = Store::new(engine, MyState { wasi: builder.build_p1() });
    let instance = linker.instantiate(&mut store, &module)?;

    if args.is_empty() {
        print_info(engine, &abs_path)?;
        println!("💡 Tip: Use 'rsxtk mod <path> <function> [args]' to execute.");
        return Ok(());
    }

    let func_name = &args[0];
    let func = instance.get_func(&mut store, func_name)
        .ok_or_else(|| anyhow!("Func '{}' not found", func_name))?;
    
    let func_type = func.ty(&store);
    let param_tys = func_type.params();
    
    let mut wasm_params = Vec::new();
    for (i, ty) in param_tys.enumerate() {
        if i + 1 >= args.len() { bail!("Missing argument for parameter {}", i); }
        let val = &args[i + 1];
        wasm_params.push(match ty {
            wasmtime::ValType::I32 => Val::I32(val.parse()?),
            wasmtime::ValType::I64 => Val::I64(val.parse()?),
            wasmtime::ValType::F32 => Val::F32(val.parse::<f32>()?.to_bits()),
            wasmtime::ValType::F64 => Val::F64(val.parse::<f64>()?.to_bits()),
            _ => bail!("Unsupported parameter type"),
        });
    }

    let mut results = vec![Val::I32(0); func_type.results().len()];
    func.call(&mut store, &wasm_params, &mut results)?;
    print_val_results(&results);
    Ok(())
}

fn execute_module(engine: &Engine, module: Module, host_dir: &Path, filename: &str) -> Result<()> {
    let mut linker: Linker<MyState> = Linker::new(engine);
    p1::add_to_linker_sync(&mut linker, |state| &mut state.wasi)?;
    
    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stdio().inherit_args();
    builder.preopened_dir(host_dir, ".", wasmtime_wasi::DirPerms::all(), wasmtime_wasi::FilePerms::all())?;
    
    let mut store = Store::new(engine, MyState { wasi: builder.build_p1() });
    let instance = linker.instantiate(&mut store, &module)?;
    
    if let Some(func) = instance.get_func(&mut store, "_start") {
        if let Err(e) = func.call(&mut store, &[], &mut []) {
            match e.downcast::<wasmtime_wasi::I32Exit>() {
                Ok(status) if status.0 == 0 => (),
                Ok(status) => bail!("Exited status: {}", status.0),
                Err(e) => return Err(e),
            }
        }
    } else {
        println!("💡 Library module loaded.");
        print_info(engine, Path::new(filename))?;
    }
    Ok(())
}

fn load_module(engine: &Engine, path: &Path) -> Result<Module> {
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    match ext {
        "rs" => {
            let artifact = build_and_cache_script(engine, path)?;
            unsafe { Ok(Module::deserialize_file(engine, artifact)?) }
        },
        "wat" => {
            let bytes = fs::read(path)?;
            Ok(Module::new(engine, &wat::parse_bytes(&bytes)?)?)
        },
        "wasm" => Ok(Module::new(engine, &fs::read(path)?)?),
        "cwasm" => unsafe { 
            Module::deserialize_file(engine, path).map_err(|e| {
                anyhow!("Machine ID Mismatch or corrupted file: {}\nTry 'rsxtk clean' and re-run.", e)
            })
        },
        _ => bail!("Unsupported file type: {}", ext),
    }
}

// --- Build & Caching ---

fn build_and_cache_script(engine: &Engine, script_path: &Path) -> Result<PathBuf> {
    let stem = script_path.file_stem().unwrap().to_str().unwrap();
    let cache_root = std::env::current_dir()?.join(".tk").join(stem);
    let cwasm_path = cache_root.join(format!("{}.cwasm", stem));
    let id_path = cache_root.join("machine.id");
    
    let current_id = get_machine_id();

    if cwasm_path.exists() && id_path.exists() {
        let cached_id = fs::read_to_string(&id_path)?;
        let src_mtime = fs::metadata(script_path)?.modified()?;
        let cache_mtime = fs::metadata(&cwasm_path)?.modified()?;
        
        if cached_id == current_id && cache_mtime > src_mtime {
            return Ok(cwasm_path);
        }
    }

    fs::create_dir_all(cache_root.join("src"))?;
    let content = fs::read_to_string(script_path)?;
    let parts: Vec<&str> = content.splitn(3, "---").collect();
    if parts.len() < 3 { 
        bail!(
            "❌ Missing script manifest in '{}'.\n\n\
            WASI scripts require a header block for dependencies.\n\
            💡 Fix: Run 'rsxtk add {} <crate>' or 'rsxtk fmt {}' to inject it.",
            script_path.display(),
            script_path.file_name().unwrap().to_str().unwrap(),
            script_path.file_name().unwrap().to_str().unwrap()
        ); 
    }

    fs::write(cache_root.join("Cargo.toml"), format!("[package]\nname=\"{}\"\nversion=\"0.1.0\"\nedition=\"2021\"\n\n{}", stem, parts[1].trim()))?;
    fs::write(cache_root.join("src/main.rs"), format!("\n{}", parts[2].trim_start()))?;

    let status = std::process::Command::new("cargo")
        .env("CARGO_TARGET_DIR", &cache_root.join("t"))
        .args(["build", "--manifest-path", cache_root.join("Cargo.toml").to_str().unwrap(), "--target", "wasm32-wasip1", "--release"])
        .status()?;

    if !status.success() { bail!("Build failed."); }
    let wasm_output = cache_root.join("t/wasm32-wasip1/release").join(format!("{}.wasm", stem));
    
    let mut module = walrus::Module::from_buffer(&fs::read(&wasm_output)?)?;
    let optimized_wasm = module.emit_wasm();
    
    let cwasm_bytes = engine.precompile_module(&optimized_wasm)?;
    fs::write(&cwasm_path, cwasm_bytes)?;
    fs::write(&id_path, current_id)?;
    
    Ok(cwasm_path)
}

// --- Utilities & Helpers ---

fn init_script(name: &str, is_mod: bool) -> Result<()> {
    let filename = if name.ends_with(".rs") { name.to_string() } else { format!("{}.rs", name) };
    let content = if is_mod {
        "---\n[dependencies]\n---\n#[no_mangle]\npub extern \"C\" fn add(a: i32, b: i32) -> i32 {\n    a + b\n}"
    } else {
        "---\n[dependencies]\n---\nfn main() {\n    println!(\"Hello from rsxtk!\");\n}"
    };
    fs::write(&filename, content)?;
    println!("✨ Initialized {}", filename);
    Ok(())
}

fn print_val_results(results: &[Val]) {
    for res in results {
        match res {
            Val::I32(v) => println!("{}", v),
            Val::I64(v) => println!("{}", v),
            Val::F32(v) => println!("{}", f32::from_bits(*v)),
            Val::F64(v) => println!("{}", f64::from_bits(*v)),
            _ => println!("{:?}", res),
        }
    }
}

fn print_info(engine: &Engine, path: &Path) -> Result<()> {
    let module = load_module(engine, path)?;
    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("unknown");
    
    println!("\n📄 Module Info: {}", filename);
    println!("────────────────────────────────────────");

    println!("🚀 User Callable Functions:");
    let mut count = 0;
    for export in module.exports() {
        if let Some(func_type) = export.ty().func() {
            count += 1;
            let params: Vec<String> = func_type.params().map(|t| t.to_string()).collect();
            let results: Vec<String> = func_type.results().map(|t| t.to_string()).collect();
            let res_str = if results.is_empty() { String::new() } else { format!(" -> {}", results.join(", ")) };
            println!("  - {}({}){}", export.name(), params.join(", "), res_str);
        }
    }
    if count == 0 { println!("  (None)"); }

    let has_wasi = module.imports().any(|i| i.module() == "wasi_snapshot_preview1" || i.module().starts_with("wasi:"));
    println!("\n🛠️  WASI Support: {}", if has_wasi { "Yes" } else { "No" });
    println!("────────────────────────────────────────\n");

    Ok(())
}

fn build_with_virtual_cargo(engine: &Engine, path: &Path) -> Result<PathBuf> {
    let abs_path = fs::canonicalize(path)?;
    let _ = build_and_cache_script(engine, &abs_path)?;
    let stem = abs_path.file_stem().unwrap().to_str().unwrap();
    let wasm_path = std::env::current_dir()?.join(".tk").join(stem).join("t/wasm32-wasip1/release").join(format!("{}.wasm", stem));
    let local_wasm = abs_path.with_extension("wasm");
    fs::copy(wasm_path, &local_wasm)?;
    println!("🔨 Built: {}", local_wasm.display());
    Ok(local_wasm)
}

fn convert_wasm_wat(input: &Path, output: Option<PathBuf>) -> Result<()> {
    let data = fs::read(input)?;
    let is_wasm = input.extension().map_or(false, |e| e == "wasm");
    let out = output.unwrap_or_else(|| input.with_extension(if is_wasm { "wat" } else { "wasm" }));
    if is_wasm { fs::write(&out, wasmprinter::print_bytes(&data)?)?; }
    else { fs::write(&out, wat::parse_bytes(&data)?)?; }
    println!("🔄 Converted to: {}", out.display());
    Ok(())
}

fn run_cargo_script(path: &Path) -> Result<()> {
    let status = std::process::Command::new("cargo").arg("-Z").arg("script").arg(fs::canonicalize(path)?).status()?;
    if !status.success() { bail!("Cargo script failed."); }
    Ok(())
}

fn edit_deps(path: &Path, crate_name: &str, version: Option<&str>) -> Result<()> {
    let content = fs::read_to_string(path)?;
    
    // Auto-detect manifest
    let (manifest, code) = if content.starts_with("---") {
        let parts: Vec<&str> = content.splitn(3, "---").collect();
        if parts.len() < 3 { bail!("❌ Invalid format: Manifest block is incomplete."); }
        (parts[1].to_string(), parts[2].to_string())
    } else {
        (String::from("[dependencies]\n"), content)
    };

    let mut lines: Vec<String> = manifest.lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with(crate_name) && t != "[dependencies]"
        })
        .map(|s| s.to_string())
        .collect();

    if let Some(v) = version {
        if !crate_name.is_empty() {
            lines.push(format!("{} = \"{}\"", crate_name, v));
        }
    }

    let new_content = format!("---\n[dependencies]\n{}\n---\n{}", lines.join("\n").trim(), code.trim_start());
    fs::write(path, new_content)?;
    
    if !crate_name.is_empty() {
        println!("✅ Manifest updated in {}", path.display());
    }
    Ok(())
}

fn get_latest_version(crate_name: &str) -> String {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .build().ok()
        .and_then(|c| c.get(format!("https://crates.io/api/v1/crates/{}", crate_name)).send().ok())
        .and_then(|res| res.json::<Value>().ok())
        .and_then(|j| j["crate"]["max_version"].as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "*".to_string())
}

fn create_resource_dir(path: Option<&Path>) -> Result<()> {
    let base = path.unwrap_or(Path::new("."));
    fs::create_dir_all(base.join("rsc"))?;
    println!("📂 Created rsc/ folder.");
    Ok(())
}

fn clean_cache() -> Result<()> {
    let cache = std::env::current_dir()?.join(".tk");
    if cache.exists() { fs::remove_dir_all(cache)?; println!("🗑️ Cache cleared."); }
    Ok(())
}

fn list_deps(path: &Path) -> Result<()> {
    let content = fs::read_to_string(path)?;
    let parts: Vec<&str> = content.splitn(3, "---").collect();
    if parts.len() >= 3 { println!("📦 Dependencies:\n{}", parts[1].trim()); }
    else { println!("⚠️ No manifest found in this script."); }
    Ok(())
}

fn format_script(path: &Path) -> Result<()> {
    let content = fs::read_to_string(path)?;
    
    // Preflight: Ensure manifest exists
    if !content.starts_with("---") {
        println!("✨ No manifest detected. Injecting '---' header...");
        edit_deps(path, "", None)?; 
    }

    let content = fs::read_to_string(path)?;
    let parts: Vec<&str> = content.splitn(3, "---").collect();
    
    let temp = path.with_extension("rs.tmp");
    fs::write(&temp, parts[2])?;
    let _ = std::process::Command::new("rustfmt").arg(&temp).status();
    
    fs::write(path, format!("---\n{}\n---\n{}", parts[1].trim(), fs::read_to_string(&temp)?))?;
    let _ = fs::remove_file(temp);
    println!("✨ Formatted {}", path.display());
    Ok(())
}

fn optimize_wasm(input: &Path, output: Option<PathBuf>) -> Result<()> {
    let bytes = fs::read(input)?;
    let mut module = walrus::Module::from_buffer(&bytes)?;
    fs::write(output.unwrap_or_else(|| input.to_path_buf()), module.emit_wasm())?;
    Ok(())
}

fn benchmark_wasm(engine: &Engine, path: &Path, iterations: u32) -> Result<()> {
    let start = Instant::now();
    for _ in 0..iterations { run_wasm(engine, path)?; }
    println!("📊 Avg Execution: {:?}", start.elapsed() / iterations);
    Ok(())
}
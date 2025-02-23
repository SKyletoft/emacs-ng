#[macro_use]
extern crate lazy_static;

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process;

use cargo_toml::Manifest;
use regex::Regex;

static C_NAME: &str = "c_name = \"";

/// Exit with error $code after printing the $fmtstr to stderr
macro_rules! fail_with_msg {
    ($code:expr, $modname:expr, $lineno:expr, $($arg:expr),*) => {{
        eprintln!("In {} on line {}", $modname, $lineno);
        eprintln!($($arg),*);
        process::exit($code);
    }};
}

pub struct LintMsg {
    modname: String,
    lineno: u32,
    msg: String,
}

impl LintMsg {
    fn new(modname: &str, lineno: u32, msg: String) -> Self {
        Self {
            modname: modname.to_string(),
            lineno: lineno,
            msg: msg,
        }
    }

    pub fn fail(self, code: i32) -> ! {
        fail_with_msg!(code, self.modname, self.lineno, "{}", self.msg);
    }
}

pub enum BuildError {
    IOError(io::Error),
    Lint(LintMsg),
}

impl From<io::Error> for BuildError {
    fn from(e: io::Error) -> Self {
        BuildError::IOError(e)
    }
}

impl From<LintMsg> for BuildError {
    fn from(e: LintMsg) -> Self {
        BuildError::Lint(e)
    }
}

#[derive(Clone)]
struct ModuleInfo {
    pub name: String,
    pub path: PathBuf,
}

impl ModuleInfo {
    pub fn from_path(mod_path: &PathBuf) -> Option<ModuleInfo> {
        // in order to parse correctly, determine where the code lives.
        // For submodules that will be in a mod.rs file.
        if mod_path.is_dir() {
            let tmp = path_as_str(mod_path.file_name()).to_string();
            let path = mod_path.join("mod.rs");
            if path.is_file() {
                return Some(ModuleInfo {
                    path: path,
                    name: tmp,
                });
            }
        } else if let Some(ext) = mod_path.extension() {
            if ext == "rs" {
                return Some(ModuleInfo {
                    path: mod_path.clone(),
                    name: path_as_str(mod_path.file_stem()).to_string(),
                });
            }
        }

        None
    }
}

struct ModuleData {
    pub info: ModuleInfo,
    pub c_exports: Vec<(Option<String>, String)>,
    pub lisp_fns: Vec<(Option<String>, String)>,
    pub protected_statics: Vec<String>,
}

impl ModuleData {
    pub fn new(info: ModuleInfo) -> Self {
        Self {
            info: info,
            c_exports: Vec::new(),
            lisp_fns: Vec::new(),
            protected_statics: Vec::new(),
        }
    }
}

struct ModuleParser<'a> {
    info: &'a ModuleInfo,
    lineno: u32,
}

impl<'a> ModuleParser<'a> {
    pub fn new(mod_info: &'a ModuleInfo) -> Self {
        ModuleParser {
            info: mod_info,
            lineno: 0,
        }
    }

    pub fn run(&mut self, in_file: impl BufRead) -> Result<ModuleData, BuildError> {
        let mut mod_data = ModuleData::new(self.info.clone());
        let mut reader = in_file.lines();
        let mut has_include = false;
        let mut preceding_cfg: Option<String> = None;

        while let Some(next) = reader.next() {
            let line = next?;
            self.lineno += 1;

            if line.starts_with(' ') {
                continue;
            }

            if line.starts_with("declare_GC_protected_static!") {
                let var = self.parse_gc_protected_static(&line)?;
                mod_data.protected_statics.push(var);
            } else if line.starts_with("#[no_mangle]") {
                if let Some(next) = reader.next() {
                    let line = next?;

                    if let Some(func) = self.parse_c_export(&line, None)? {
                        self.lint_nomangle(&line)?;
                        mod_data.c_exports.push((preceding_cfg, func));
                    }

                    preceding_cfg = None;
                } else {
                    self.fail(1, "unexpected end of file");
                }
            } else if line.starts_with("#[cfg") {
                preceding_cfg = Some(line);
            } else if line.starts_with("#[lisp_fn") {
                let line = if line.ends_with("]") {
                    line.clone()
                } else {
                    let mut line = line.clone();
                    loop {
                        if let Some(next) = reader.next() {
                            let l = next?;
                            if !l.ends_with(")]") {
                                line += &l;
                            } else {
                                line += &l;
                                break;
                            }
                        } else {
                            break;
                        }
                    }

                    line
                };

                let name = if let Some(begin) = line.find(C_NAME) {
                    let start = begin + C_NAME.len();
                    let end = line[start..].find('"').unwrap() + start;
                    let name = line[start..end].to_string();
                    if name.starts_with('$') {
                        // Ignore macros, nothing we can do with them
                        continue;
                    }

                    Some(name)
                } else {
                    None
                };

                if let Some(next) = reader.next() {
                    let line = next?;

                    if let Some(func) = self.parse_c_export(&line, name)? {
                        mod_data.lisp_fns.push((preceding_cfg, func));
                    }
                } else {
                    self.fail(1, "unexpected end of file");
                }

                preceding_cfg = None;
            } else if line.starts_with("#[async_stream") {
                if let Some(next) = reader.next() {
                    let line = next?;

                    if let Some(func) = self.parse_c_export(&line, None)? {
                        let mut prefix = String::from("call_");
                        prefix.push_str(&func);
                        mod_data.lisp_fns.push((preceding_cfg, prefix));
                    }
                } else {
                    self.fail(1, "Unexpected end of file");
                }

                preceding_cfg = None;
            } else if line.starts_with("include!(concat!(") {
                has_include = true;
            } else if line.starts_with("/*") && !line.ends_with("*/") {
                while let Some(next) = reader.next() {
                    let line = next?;
                    if line.ends_with("*/") {
                        break;
                    }
                }
            } else {
                preceding_cfg = None;
            }
        }

        if !has_include && !(mod_data.lisp_fns.is_empty() && mod_data.protected_statics.is_empty())
        {
            let msg = format!(
                "{} is missing the required include for protected statics or lisp_fn exports.",
                path_as_str(self.info.path.file_name()).to_string()
            );

            self.fail(2, &msg);
        }

        Ok(mod_data)
    }

    fn fail(&mut self, code: i32, msg: &str) -> ! {
        fail_with_msg!(code, &self.info.name, self.lineno, "{}", msg);
    }

    /// Handle both no_mangle and lisp_fn functions
    fn parse_c_export(
        &mut self,
        line: &str,
        name: Option<String>,
    ) -> Result<Option<String>, LintMsg> {
        let name = self.validate_exported_function(name, line, "function must be public.")?;
        if let Some(func) = name {
            Ok(Some(func))
        } else {
            Ok(None)
        }
    }

    fn parse_gc_protected_static(&mut self, line: &str) -> Result<String, LintMsg> {
        lazy_static! {
            static ref RE: Regex = Regex::new(r#"GC_protected_static!\((.+), .+\);"#).unwrap();
        }

        match RE.captures(line) {
            Some(caps) => {
                let name = caps[1].to_string();
                Ok(name)
            }
            None => Err(LintMsg::new(
                &self.info.name,
                self.lineno,
                "could not parse protected static".to_string(),
            )),
        }
    }

    // Determine if a function is exported correctly and return that function's name or None.
    fn validate_exported_function(
        &mut self,
        name: Option<String>,
        line: &str,
        msg: &str,
    ) -> Result<Option<String>, LintMsg> {
        match name.or_else(|| get_function_name(line)) {
            Some(name) => {
                if line.starts_with("pub ") {
                    Ok(Some(name))
                } else if line.starts_with("fn ") {
                    Err(LintMsg::new(
                        &self.info.name,
                        self.lineno,
                        format!("\n`{}` is not public.\n{}", name, msg),
                    ))
                } else {
                    eprintln!(
                        "Unhandled code in the {} module at line {}",
                        self.info.name, self.lineno
                    );
                    unreachable!();
                }
            }
            None => Ok(None),
        }
    }

    fn lint_nomangle(&mut self, line: &str) -> Result<(), LintMsg> {
        if !(line.starts_with("pub extern \"C\" ") || line.starts_with("pub unsafe extern \"C\" "))
        {
            Err(LintMsg::new(
                &self.info.name,
                self.lineno,
                "'no_mangle' functions exported for C need 'extern \"C\"' too.".to_string(),
            ))
        } else {
            Ok(())
        }
    }
}

// Parse the function name out of a line of source
fn get_function_name(line: &str) -> Option<String> {
    if let Some(pos) = line.find('(') {
        if let Some(fnpos) = line.find("fn ") {
            let name = line[(fnpos + 3)..pos].trim();
            return Some(name.to_string());
        }
    }

    None
}

fn handle_file(mod_path: &PathBuf) -> Result<Option<ModuleData>, BuildError> {
    if let Some(mod_info) = ModuleInfo::from_path(mod_path) {
        let fp = match File::open(mod_info.path.clone()) {
            Ok(f) => f,
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("Failed to open {}: {}", mod_info.path.to_string_lossy(), e),
                )
                .into());
            }
        };

        let mut parser = ModuleParser::new(&mod_info);
        let mod_data = parser.run(BufReader::new(fp))?;
        Ok(Some(mod_data))
    } else {
        Ok(None)
    }
}

// Transmute &OsStr to &str
fn path_as_str(path: Option<&OsStr>) -> &str {
    path.and_then(|p| p.to_str())
        .unwrap_or_else(|| panic!("Cannot understand string: {:?}", path))
}

pub fn env_var(name: &str) -> String {
    env::var(name).unwrap_or_else(|e| panic!("Could not find {} in environment: {}", name, e))
}

// What to ignore when walking the list of files
fn ignore(path: &str, additional_ignored_paths: &Vec<&str>) -> bool {
    path == ""
        || path.starts_with('.')
        || path == "lib.rs"
        || additional_ignored_paths.contains(&path)
}

/// Find modules in PATH which should contain the src directory of a crate
fn find_crate_modules(path: &PathBuf) -> Result<Vec<ModuleData>, BuildError> {
    let mut modules: Vec<ModuleData> = Vec::new();
    let ignored_paths = vec!["lib.rs"];

    let in_path: PathBuf = path.iter().collect();
    for entry in fs::read_dir(in_path)? {
        let mod_path = entry?.path();

        if !ignore(path_as_str(mod_path.file_name()), &ignored_paths) {
            if let Some(mod_data) = handle_file(&mod_path)? {
                modules.push(mod_data);
            }
        }
    }

    Ok(modules)
}

/// Lookup public functions in a crate's modules and add the declarations
/// to the c_exports file that is determined by out_file.
fn generate_crate_c_export_file(
    mut out_file: &File,
    modules: &Vec<ModuleData>,
) -> Result<(), BuildError> {
    for mod_data in modules {
        for (cfg, func) in &mod_data.c_exports {
            if let Some(cfg) = cfg {
                write!(out_file, "{}\n", cfg)?;
            }
            write!(
                out_file,
                "pub use crate::{}::{};\n",
                mod_data.info.name, func
            )?;
        }
        for (cfg, func) in &mod_data.lisp_fns {
            if let Some(cfg) = cfg {
                write!(out_file, "{}\n", cfg)?;
            }
            write!(
                out_file,
                "pub use crate::{}::F{};\n",
                mod_data.info.name, func
            )?;
        }
    }
    write!(out_file, "\n")?;

    Ok(())
}

/// First we have to generate the include file for the main crate which
/// will be stored in OUT_DIR. It only contains the rust_init_syms
/// that runs the crates *_init_syms functions.
pub fn generate_include_files(crates_dir: PathBuf) -> Result<(), BuildError> {
    let out_path: PathBuf = [&env_var("OUT_DIR")].iter().collect();
    let mut out_file = File::create(out_path.join("c_exports.rs"))?;

    // Add main rust_init_syms function to the main c_exports file
    write!(
        out_file,
        "#[no_mangle]\npub extern \"C\" fn rust_init_syms() {{\n"
    )?;

    // Iterate crates path and generate include files
    for entry in fs::read_dir(crates_dir).unwrap() {
        let crate_path = entry?.path();

        if build_ignored_crates(&crate_path) {
            continue;
        };
        // generate_crate_exports(&crate_path)?;
        let crate_name = get_crate_name(&crate_path);

        // Call a crate's init_syms function in the main c_exports file
        let crate_init_syms = format!("{}::{}_init_syms();\n", crate_name, crate_name);
        write!(out_file, "{}", crate_init_syms)?;
    }

    write!(out_file, "}}\n")?;

    Ok(())
}

/// Return true when crate is supposed to be ignored.
/// The list of ignored crates depends on the activated features.
fn build_ignored_crates(path: &PathBuf) -> bool {
    let mut ignored_crates = vec!["lisp_util", "lisp_macros", "emacs"];

    #[cfg(not(feature = "libgit"))]
    ignored_crates.push("git");

    #[cfg(not(feature = "javascript"))]
    ignored_crates.push("js");

    #[cfg(not(feature = "ng-module"))]
    ignored_crates.push("ng_module");

    #[cfg(not(feature = "window-system-webrender"))]
    ignored_crates.push("webrender");

    let crate_path = path_as_str(path.file_name()).to_string();

    for ignored in ignored_crates {
        if ignored == crate_path {
            return true;
        }
    }

    false
}

/// Create c_exports.rs that holds a crate's generated bindings.
/// We call generate_crate_c_export_file to add regular functions bindings
/// and write_lisp_fns to create the include file for each module which holds
/// the lisp_fns.
pub fn generate_crate_exports(path: &PathBuf) -> Result<(), BuildError> {
    let modules = find_crate_modules(&path.join("src"))?;

    let _ = fs::create_dir(path.join("out"));
    let mut out_file = File::create(path.join("out").join("c_exports.rs"))?;
    generate_crate_c_export_file(&out_file, &modules)?;

    let crate_name = get_crate_name(path);
    write!(
        out_file,
        "#[no_mangle]\npub extern \"C\" fn {}_init_syms() {{\n",
        crate_name
    )?;

    write_lisp_fns(&path.join("out"), &out_file, &modules)?;

    write!(out_file, "}}\n")?;

    Ok(())
}

fn get_crate_name(path: &PathBuf) -> String {
    let manifest = Manifest::from_path(path.join("Cargo.toml")).unwrap();
    match manifest.package {
        Some(package) => package.name,
        None => path_as_str(path.file_name()).to_string(),
    }
}

/// Export lisp functions defined in rust by using the macro `export_lisp_fns`
/// Add *_init_syms function of each module to the c_exports OUT_FILE
fn write_lisp_fns(
    crate_path: &PathBuf,
    mut out_file: &File,
    modules: &Vec<ModuleData>,
) -> Result<(), BuildError> {
    for mod_data in modules {
        let exports_path: PathBuf = crate_path.join([&mod_data.info.name, "_exports.rs"].concat());

        // Start with a clean slate
        if exports_path.exists() {
            fs::remove_file(&exports_path)?;
        }

        // Add lisp_fns
        if !mod_data.lisp_fns.is_empty() {
            let mut file = File::create(&exports_path)?;
            write!(
                file,
                "export_lisp_fns! {{\n    {}\n}}\n",
                mod_data
                    .lisp_fns
                    .iter()
                    .map(|lisp_fn| match lisp_fn {
                        (Some(cfg), func) => format!("{} {}", cfg, func),
                        (_, func) => format!("{}", func),
                    })
                    .collect::<Vec<String>>()
                    .join(",\n    ")
            )?;

            write!(out_file, "    {}::rust_init_syms();\n", mod_data.info.name)?;
        }

        // Add protected_statics
        if !mod_data.protected_statics.is_empty() {
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .append(true)
                .open(exports_path)?;
            write!(
                file,
                "protect_statics_from_GC! {{ {} }}\n",
                mod_data.protected_statics.join(", ")
            )?;

            write!(
                out_file,
                "    {}::rust_static_syms();\n",
                mod_data.info.name
            )?;
        }
    }

    Ok(())
}

use std::fs::{read_to_string, File, write, copy};
use std::io::{Write, BufReader, BufRead, Error, ErrorKind};
use std::path;
use serde_json;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use clap::Parser;
use reqwest;
use tar::Archive;
use flate2::read::GzDecoder;
use std::process::{Command, Output, Stdio};
use walkdir::WalkDir;
use glob::glob;
use regex::Regex;
use colored::*;
use dirs::home_dir;

// Search for a pattern in a file and display the lines that contain it.
#[derive(Parser)]
struct Args {
    #[clap(long = "snapshot", short = 's', default_value = "none", help = "Defines the date of the Arch-repository snapshot that should be used. Always enter in the format YYYY_MM_DD. If no date is entered, no update will be performed.")]
    snapshot: String,

    #[clap(long = "pacconfig", short = 'p', default_value = "none")]
    pacconfig: String,

    #[clap(long = "config", short = 'c', default_value = "~/.config/nompac_rs/configs/config.json")]
    config: String,

    #[clap(long = "packagegroups", short = 'g', default_value = "none")]
    package_groups: String,

}

#[derive(Debug, Deserialize, Serialize)]
struct Config {
    name: String,
    build_dir: String,
    patch_dir: String,
    overlay_dir: String,
    local_repo_dir: String,
    packages: Vec<HashMap<String, Vec<String>>>,
    patches: Vec<HashMap<String, Vec<String>>>,
    overlays: Vec<String>,
    packagegroups: String,
    pacconfig: String,
    snapshot: String
}

fn get_current_version_from_repo(package_name: &str) -> Result<String, Box<dyn std::error::Error>> {
    //! read current package version from repository
    //! takes package name and returns version-revision
    let url = format!("https://gitlab.archlinux.org/archlinux/packaging/packages/{}/-/raw/main/PKGBUILD", package_name);    
    // is the url accessable?
    let response = reqwest::blocking::get(url)?;
    // read the response body as string
    let body = response.text()?;
    get_version_from_pkgbuild(&body)

}

fn get_version_from_overlay(overlay_dir: &str, package_name: &str) -> Result<String, Box<dyn std::error::Error>> {
    //! takes overlay directory and package name and returns version-revision
    let url = format!("{}/{}/PKGBUILD", overlay_dir, package_name);
    let contents = read_to_string(url)?;
    get_version_from_pkgbuild(&contents)
}

fn get_version_from_pkgbuild(file_contents: &str) -> Result<String, Box<dyn std::error::Error>> {
//! Extract version from pkgbuild-file that was given as string in file_contents

    let mut pkgver_row: Vec<&str> = Vec::new();
    for line in file_contents.lines() {
        if line.starts_with("pkgver=") {
            pkgver_row.append(&mut line.split("=").collect());
        }
        if line.starts_with("pkgrel=") {
            pkgver_row.append(&mut line.split("=").collect());
        }
    }
    Ok(format!("{}-{}", pkgver_row[1].to_string(), pkgver_row[3].to_string()))
}

fn get_current_tarball_from_repo(package_name: &str, package_version: &str, file_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    //! fetch the tarball from arch online repository.
    //! parameters: package_name, package_version, file_path

    let url = format!(
        "https://gitlab.archlinux.org/archlinux/packaging/packages/{}/-/archive/{}/{}.tar.gz",
        package_name,
        package_version,
        format!("{}-{}",package_name,package_version)
    );

    let resp = reqwest::blocking::get(&url)?;

    let mut out_file = std::fs::File::create(file_path)?;
    let body = resp.bytes()?;
    out_file.write_all(&body)?;

    println!("Successfully downloaded {}-{}.tar.gz", package_name, package_version);

    Ok(())
}

fn extract_tgz(filename: &str, output_path: &str) -> Result<(), std::io::Error> {
    //! takes filename to be extracted and the output path
    //! extract a tar.gz file
    let file = File::open(filename)?;
    let reader = BufReader::new(GzDecoder::new(file));

    let mut archive = Archive::new(reader);

    archive.unpack(output_path)?;

    Ok(())
}

fn modify_pkgbuild(file: &str, patch: &str) -> Result<(), std::io::Error> {
//! takes PKBBUILD file and patchname and adds the patch to the file
    let mut block_state = format!("{}", "none");
    let mut prepare_block_exists = false;

    // Read the contents of the file into a String
    let content = read_to_string(file)?;
    // Initialize modified_content as an empty String
    let mut modified_content: String = String::new();

    let mut lines = content.lines();

    while let Some(line) = lines.next() {
        
        if line.trim_start().starts_with("source") {
            block_state = format!("{}","source");
        }
        if line.trim_start().starts_with("prepare") {
            block_state = format!("{}","prepare");
            prepare_block_exists = true;
        }
        if block_state == "source" && line.trim_start().ends_with(")") {
            block_state = format!("{}", "none");
            modified_content.push_str(&format!("    \"{}\"\n", patch));
        }
        if block_state == "prepare" && line.trim_start().ends_with("}") {
            block_state = format!("{}", "none");
            modified_content.push_str(&format!("    patch -Np1 -i \"${{srcdir}}/{}\"\n", patch));
        }
        modified_content.push_str(&format!("{}\n", line));

    }

    // if no prepare block exists in the PKGBUILD, append the block with the patch command
    if prepare_block_exists == false {
        modified_content.push_str(&format!("\nprepare() {{\n    cd wlroots-\"${{pkgver}}\"\n    patch -Np1 -i \"${{srcdir}}/{}\"\n}}\n", patch));
    }

    // Write the entire modified content back to the file in one step
    write(file, &modified_content)?;
    
    Ok(())
}

fn apply_patches(config: &Config, patches: &Vec<String>, packagename: &str, packageversion: &str) {
    //! funtion takes the configuration, a vector of packages, the package name for that the and
    //! the package version
    //! patches should be applied and the path to the PKBBUILD file.
    //! Then the function modifies the PKGBUILD file.
    for patch in patches {
        let pkg_build_dir = format!("{}/src/{}-{}/", config.build_dir, packagename, packageversion);
        let _ = copy(format!("{}/{}/{}", config.patch_dir, packagename, patch),format!("{}/{}", pkg_build_dir, patch));
        let _ = modify_pkgbuild(&format!("{}/PKGBUILD", pkg_build_dir), &patch);
    }
}

fn run_commands_stdout(commands: Vec<&str>) -> Result<Output, std::io::Error> {
    //! takes vector of bash commands and executes the commands
    let joined_command = commands.join("; ");
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
       .arg(joined_command)
       .output()
}

fn run_commands(commands: Vec<&str>) {
    //! takes vector of bash commands and executes the commands
    let joined_command = commands.join("; ");
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(joined_command)
        .spawn()
        .expect("Error: Failed to run editor")
        .wait()
        .expect("Error: Editor returned a non-zero status");
}

fn run_commands_piped(commands: Vec<&str>) -> Result<(), Error> {
    //! takes vektor or bash commands and executes the commands while reading stdio as buffer and
    //! prints stdout in realtime
    let joined_command = commands.join("; ");

    let cmd = Command::new("bash")
        .arg("-c")
        .arg(joined_command)
        .stdout(Stdio::piped())
        .spawn()?
        .stdout
        .ok_or_else(|| Error::new(ErrorKind::Other, "Could not capture standard output."))?;

    let reader = BufReader::new(cmd);

    reader
        .lines()
        .filter_map(|line| line.ok())
        .for_each(|line| println!("{}", line));

    Ok(())
}

fn build_package(pkg_build_dir: &str)  {
//! takes the src-directory of the build files and executes a bash proces to
//! build the package
    let tmp_command = format!("pushd {}", pkg_build_dir);
    println!("{}", tmp_command);

    let commands = vec![
        &tmp_command,
        "updpkgsums",
	"makepkg -cCsr --skippgpcheck",
	"popd"
    ];

    let _ = run_commands_piped(commands);

}

fn update_repository(config: &Config, packagename: &str) -> Result<(), Box<dyn std::error::Error>> {    
//! takes config struct and packagename and updates the repository so that a build package is
//! copied to the local repository directory and added to the directory
    for entry_result in glob(&format!("{}/src/{}/**/*.txt", config.build_dir, packagename))? {

        match entry_result {
            Ok(entry) => {
                if let Some(file_name) = entry.as_path().file_name() {
                    let _ = copy(entry.as_path(),&format!("{}/{}", config.local_repo_dir, file_name.to_string_lossy()));	
                    let tmp_command = format!("repo-add {}/nomispaz.db.tar.zst {}/{}", config.local_repo_dir, config.local_repo_dir, file_name.to_string_lossy());

                    let commands: Vec<&str> = vec![
                        &tmp_command
                    ];

                    let _ = run_commands_stdout(commands);

                }
            }
            Err(err) => eprintln!("Error reading entry: {:?}", err),
        }

    }

    Ok(())
}

fn cleanup(config: &Config) {
    //! cleans the build directory

    let tmp_command = format!("rm -r {}/src", config.build_dir);

    let commands: Vec<&str> = vec![
        &tmp_command
    ];

    let _ = run_commands_stdout(commands);
}

fn get_installed_version(packagename: &str) -> Result<String, String> {
//! takes the package name and returns version-revision of the installed package
	// Construct the command
        let tmp_command = format!("pacman -Q | grep {} | cut -d' ' -f 2", packagename);

        let commands: Vec<&str> = vec![
            &tmp_command
        ];

        match run_commands_stdout(commands) {
            Ok(result) => {
                if result.stdout.len() > 0 {
                    Ok(String::from_utf8_lossy(&result.stdout).into_owned())
                } else {
                    Err(format!("No version found for package {}", packagename))
                }
            }
            Err(e) => Err(format!("Error while reading package version of {}: {}", packagename,
e))
        }
}

fn modify_file(filename: &str, pattern: &str, replacement: &str) -> Result<(), Box<dyn std::error::Error>> {
//! Replace a row in filename containing the pattern with replacement.
//! The pattern needs to be given as regex
//! be mindfull of special characters in the pattern, especially rust specifics
//! $$      Match single dollar sign.

    let content = read_to_string(filename)?;

    let re = Regex::new(pattern)?;
    let modified_content = re.replace_all(&content, replacement);

    write(filename, modified_content.to_string())?;

    Ok(())
}

fn load_config_from_file(file_path: &str) -> Config {
    //! takes the path to the config file, parses the json files and returns a config struct
    let content = read_to_string(file_path).expect("Failed to read JSON configfile");
    serde_json::from_str(&content).expect("Errors in the JSON-structure of the configfile")
}

fn main() {

    // define and read command line arguments
    let args = Args::parse();

    //if path of config-file contains ~ or $HOME, parse to the real home dir
    let home_dir: String = home_dir().unwrap().display().to_string();
    
    let mut path_to_config = args.config;
    //let mut path_to_config = format!("{}/.config/nompac_rs/config/config.json", home_dir);

    if path_to_config.trim().starts_with("~") {
        path_to_config = path_to_config.replace("~", &home_dir);
    }

    if path_to_config.trim().starts_with("~") {
        path_to_config = path_to_config.replace("$HOME", &home_dir);
    }

    // Path to JSON config file
    let configs = load_config_from_file(&path_to_config);
    
    let mut snapshot_year = "none";
    let mut snaphot_month = "none";
    let mut snapshot_day = "none";

    let mut date: Vec<String> = Vec::new();

    // if a snapshot was defined in the arguments, replace the one from the config file
    if args.snapshot == "none" {
        date = configs.snapshot.split('_').map(|s| s.to_string()).collect();
        if let [year, month, day] = &date[..] {
            snapshot_year = year;
            snaphot_month = month;
            snapshot_day = day;
        }
    }
    else {
        date = args.snapshot.split('_').map(|s| s.to_string()).collect();
        if let [year, month, day] = &date[..] {
            snapshot_year = year;
            snaphot_month = month;
            snapshot_day = day;
        }
    }

    // use package group from args if available
    let mut package_groups: Vec<String> = Vec::new();
        
    if args.package_groups == "none" {
        package_groups = configs.packagegroups.split(',').map(|s| s.to_string()).collect();    }
    else {
        package_groups = args.package_groups.split(',').map(|s| s.to_string()).collect();
    }

    // use pacconfig from args if available
    let mut pacconfig: String = String::new();
        
    if args.pacconfig == "none" {
        pacconfig = configs.pacconfig.clone();    
    }
    else {
        pacconfig = args.pacconfig.clone();
    }

    // create package list of packages that should be installed explicitely       
    let mut package_list: Vec<String> = Vec::new();

    for group in configs.packages[0].keys() {
        if package_groups.contains(group) || package_groups == ["all"] {
            for package in &configs.packages[0][group] {
                package_list.push(package.to_string().to_lowercase());
             }
        }
    }
    
    // get list of explicitely installed packages
    let mut package_list_installed: Vec<String> = Vec::new();

    match run_commands_stdout(vec!["pacman -Qe | cut -d' ' -f 1"]) {
        Ok(output) => {
            let packages = String::from_utf8_lossy(&output.stdout).to_string();
            package_list_installed = packages.split("\n").map(|s| s.to_string()).collect();
        }
        Err(e) => eprintln!("Error running commands: {}", e),
    }
   
    package_list.sort();

    // search for packages that are installed but not in package_list
    // for this, iterate over list and remove the package already read from the vector
    let mut packages_to_remove: Vec<String> = Vec::new();

    for package in &package_list_installed {
        if !package_list.contains(&package) {
            packages_to_remove.push(package.to_string());
        }
    }

    // search for packages that are in the config file but not explicitely installed
    let mut packages_to_install: Vec<String> = Vec::new();

    for package in package_list {
        if !package_list_installed.contains(&package) {
            packages_to_install.push(package);
        }
    }

    println!("{}","Used settings:".blue());
    println!("Used config file: {}", path_to_config);
    println!("Local build directory: {}", configs.build_dir);
    println!("Local repository: {}", configs.local_repo_dir);
    println!("Patch directory: {}", configs.patch_dir);
    println!("Overlay directory: {}", configs.overlay_dir);
    println!("pacman.conf location: {}", pacconfig);
    println!("Snaphot date: {}_{}_{}", snapshot_year, snaphot_month, snapshot_day);

    println!("{}", "\nBuilding patched upstream-packages".blue());

    // create necessary directories
    // build directory
    let _ = std::fs::create_dir_all(format!("{}/src", configs.build_dir));

    // apply patches, build new package and update local repository
    for package in configs.patches[0].keys() {

        let mut package_version_repo: String = String::new();
        let mut package_version_installed: String = String::new();

        match get_current_version_from_repo(&package) {
            Ok(version) => package_version_repo = version,
            Err(e) => println!("{}", format!("Package version in repository of package {} couldn't be determined: {}", package, e).red())
        }
        match get_installed_version(&package) {
            Ok(version) => package_version_installed = version,
            Err(e) => println!("{}", format!("Package version of installed package {} couldn't be determined: {}", package, e).red())
        }
       
        //only procede if the package was updated upstream
        if package_version_installed.trim() != package_version_repo.trim() {
            let _ = get_current_tarball_from_repo(&package, &package_version_repo, &format!("{}/{}-{}.tar.gz", configs.build_dir, package, package_version_repo));

            println!("{}/{}-{}.tar.gz", &configs.build_dir, &package, &package_version_repo);
            let _ = extract_tgz(&format!("{}/{}-{}.tar.gz", &configs.build_dir, &package, &package_version_repo), &format!("{}/src/", &configs.build_dir));

            apply_patches(&configs, &configs.patches[0][package], &package, &package_version_repo);
            
            build_package(&format!("{}/src/{}-{}/", configs.build_dir, package, package_version_repo));

            let _ =  update_repository(&configs, &package);

            cleanup(&configs);
        }
        else {
            println!("{}", format!("Package {} already up to date.", package).green());
        }
    }

    // build packages from overlays
    println!("{}", "\nBuilding packages from overlay".blue());

    for package in &configs.overlays {
        
        let mut package_version_overlay: String = String::new();
        let mut package_version_installed: String = String::new();

        match get_installed_version(&package) {
            Ok(version) => package_version_installed = version,
            Err(e) => println!("{}", format!("Package version of installed package {} couldn't be determined: {}", package, e).red())
        }
       
        match get_version_from_overlay(&configs.overlay_dir, &package) {
            Ok(version) => package_version_overlay = version,
            Err(e) => println!("{}", format!("Package version of package {} from overlay couldn't be determined: {}", package, e).red())
        } 

        if package_version_installed.trim() != package_version_overlay.trim() {
            // copy necessary files from overlay to build directory
            for entry in WalkDir::new(&format!("{}/{}", configs.overlay_dir, package)).into_iter().filter_map(|entry| entry.ok()) {
                if entry.path().is_file() {
                    let _ = std::fs::create_dir_all(format!("{}/src/{}/", configs.build_dir, package));
                    let _ = copy(entry.path(), format!("{}/src/{}/{}", configs.build_dir, package, entry.file_name().to_str().unwrap()));
                    println!("{}/src/{}/", configs.build_dir, package);
                }
            }

            // build the package
            build_package(&format!("{}/src/{}/", configs.build_dir, package));

            let _ = update_repository(&configs, &package);

            cleanup(&configs);
        }
        else {
            println!("{}", format!("Package {} already up to date.", package).green());
        }
    }

    // perform system update
    if snapshot_year != "none" {
                
        // update snapshot that will be used for the update
        let _ = modify_file(&format!("{}/mirrorlist", path_to_config.rsplit_once("/").unwrap().0), ".*archive.archlinux.org.*", &format!("Server = https://archive.archlinux.org/repos/{}/{}/{}/$$repo/os/$$arch", snapshot_year, snaphot_month, snapshot_day));
                
        // only perform if packages have to be removed
        if packages_to_remove.len() > 0 {
            println!("{}", "Removing the following packages since they don't exist in the config file:".red());
            let mut command: Vec<&str> = Vec::new();
            let mut package_list: String = String::new();
            for package in packages_to_remove {
                package_list.push_str(" ");
                package_list.push_str(&package);
            }
            let tmp_command = &format!("sudo pacman -Rsn {}", package_list);
            command.push(tmp_command);
            println!("{}",format!("{}", package_list).red());
            let _ = run_commands_piped(command);
        } 

        // only perform if packages have to be installed
        if packages_to_install.len() > 0 {
            println!("{}", packages_to_install.len());
            println!("{}", "Installing the following packages:".blue());
            let mut command: Vec<&str> = Vec::new();
            let mut package_list: String = String::new();
            for package in packages_to_install {
                package_list.push_str(" ");
                package_list.push_str(&package);
            }
            let tmp_command = &format!("sudo pacman -Syu {} --config {}", package_list, pacconfig);
            command.push(tmp_command);
            println!("{}",format!("{}", package_list).blue());
            //command.push("sudo DIFFPROG='nvim -d' pacdiff");
            let _ = run_commands_piped(command);
            run_commands(vec!["sudo DIFFPROG='nvim -d' pacdiff"]);
        }
        else {
            println!("{}", "Starting system update.\n".blue());
            let mut command: Vec<&str> = Vec::new();
            let tmp_command = &format!("sudo pacman -Syu --config {}", pacconfig);
            command.push(tmp_command);
            command.push("sudo DIFFPROG='nvim -d' pacdiff");
            let _ = run_commands_piped(command);
            run_commands(vec!["sudo DIFFPROG='nvim -d' pacdiff"]);
        }
        //println!("{}", format!("Execute --- sudo DIFFPROG='nvim -d' pacdiff --- after the update").red());
    }
}

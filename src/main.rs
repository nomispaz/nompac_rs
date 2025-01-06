use clap::Parser;
use colored::*;
use dirs::home_dir;
use flate2::read::GzDecoder;
use glob::glob;
use regex::Regex;
use reqwest;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{copy, read_to_string, write, File};
use std::io::{stdin, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use tar::Archive;
use toml::{self, Table};
use walkdir::WalkDir;
//async
use std::sync::mpsc::{self, Sender};
use std::thread;

// Search for a pattern in a file and display the lines that contain it.
#[derive(Parser)]
struct Args {
    #[clap(
        long = "snapshot",
        short = 's',
        default_value = "none",
        help = "Defines the date of the Arch-repository snapshot that should be used. Always enter in the format YYYY_MM_DD. If no date is entered, no update will be performed."
    )]
    snapshot: String,

    #[clap(long = "pacconfig", short = 'p', default_value = "none")]
    pacconfig: String,

    #[clap(
        long = "config",
        short = 'c',
        default_value = "~/.config/nompac/configs/config.toml"
    )]
    config: String,

    #[clap(long = "initiate", short = 'i', default_value = "no")]
    initiate: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct Config {
    name: String,
    build_dir: String,
    patch_dir: String,
    overlay_dir: String,
    local_repo: String,
    packages: Vec<String>,
    patches: Vec<HashMap<String, Vec<String>>>,
    overlays: Vec<String>,
    pacconfig: String,
    mirrorlist: String,
    snapshot: String,
    configs: Vec<SystemConfigs>,
    imports: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct SystemConfigs {
    path: String,
    config_entry: Vec<ConfigEntry>,
}

impl IntoIterator for SystemConfigs {
    type Item = ConfigEntry;
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.config_entry.into_iter()
    }
}

impl Clone for SystemConfigs {
    fn clone(&self) -> Self {
        SystemConfigs {
            path: self.path.clone(),
            config_entry: self.config_entry.clone(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct ConfigEntry {
    orig: String,
    changed: String,
}

impl Clone for ConfigEntry {
    fn clone(&self) -> Self {
        ConfigEntry {
            orig: self.orig.clone(),
            changed: self.changed.clone(),
        }
    }
}

fn get_current_version_from_repo(package_name: &str) -> Result<String, Box<dyn std::error::Error>> {
    //! read current package version from repository
    //! takes package name and returns version-revision
    let url = format!(
        "https://gitlab.archlinux.org/archlinux/packaging/packages/{}/-/raw/main/PKGBUILD",
        package_name
    );
    // is the url accessable?
    let response = reqwest::blocking::get(url)?;
    // read the response body as string
    let body = response.text()?;
    get_version_from_pkgbuild(&body)
}

fn get_version_from_overlay(
    overlay_dir: &str,
    package_name: &str,
) -> Result<String, Box<dyn std::error::Error>> {
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
    Ok(format!(
        "{}-{}",
        pkgver_row[1].to_string(),
        pkgver_row[3].to_string()
    ))
}

fn get_current_tarball_from_repo(
    package_name: &str,
    package_version: &str,
    file_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    //! fetch the tarball from arch online repository.
    //! parameters: package_name, package_version, file_path

    let url = format!(
        "https://gitlab.archlinux.org/archlinux/packaging/packages/{}/-/archive/{}/{}.tar.gz",
        package_name,
        package_version,
        format!("{}-{}", package_name, package_version)
    );

    let resp = reqwest::blocking::get(&url)?;

    let mut out_file = std::fs::File::create(file_path)?;
    let body = resp.bytes()?;
    out_file.write_all(&body)?;

    println!(
        "Successfully downloaded {}-{}.tar.gz",
        package_name, package_version
    );

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

fn modify_pkgbuild(file: &str, patch: &str, package_name: &str) -> Result<(), std::io::Error> {
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
            block_state = format!("{}", "source");
        }
        if line.trim_start().starts_with("prepare") {
            block_state = format!("{}", "prepare");
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
        modified_content.push_str(&format!("\nprepare() {{\n    cd {package_name}-\"${{pkgver}}\"\n    patch -Np1 -i \"${{srcdir}}/{}\"\n}}\n", patch));
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
        let pkg_build_dir = format!(
            "{}/src/{}-{}/",
            config.build_dir, packagename, packageversion
        );
        let _ = copy(
            format!("{}/{}/{}", config.patch_dir, packagename, patch),
            format!("{}/{}", pkg_build_dir, patch),
        );
        let _ = modify_pkgbuild(&format!("{}/PKGBUILD", pkg_build_dir), &patch, packagename);
    }
}

fn build_package(pkg_build_dir: &str) {
    //! takes the src-directory of the build files and executes a bash process to
    //! build the package

    let commands: Vec<String> = vec![
        format!("pushd {}", pkg_build_dir),
        "updpkgsums".to_string(),
        "makepkg -cCsr --skippgpcheck".to_string(),
        "popd".to_string(),
    ];

    create_cmd_thread(commands, true);
}

fn update_repository(
    config: &Config,
    local_repo_dir: &str,
    packagename: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    //! takes config struct and packagename and updates the repository so that a build package is
    //! copied to the local repository directory and added to the directory
    for entry_result in glob(&format!(
        "{}/src/{}*/**/*.pkg.tar.zst",
        config.build_dir, packagename
    ))? {
        match entry_result {
            Ok(entry) => {
                if let Some(file_name) = entry.as_path().file_name() {
                    let _ = copy(
                        entry.as_path(),
                        &format!("{}/{}", local_repo_dir, file_name.to_string_lossy()),
                    );
                    let tmp_command = format!(
                        "repo-add -R {}/nomispaz.db.tar.zst {}/{}",
                        local_repo_dir,
                        local_repo_dir,
                        file_name.to_string_lossy()
                    );

                    let commands: Vec<&str> = vec![&tmp_command];

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

    let commands: Vec<&str> = vec![&tmp_command];

    let _ = run_commands_stdout(commands);
}

fn get_installed_version(packagename: &str) -> Result<String, String> {
    //! takes the package name and returns version-revision of the installed package
    // Construct the command
    let tmp_command = format!(
        "pacman -Q | grep \"\\<{}\\>\" | cut -d' ' -f 2",
        packagename
    );

    let commands: Vec<&str> = vec![&tmp_command];

    match run_commands_stdout(commands) {
        Ok(result) => {
            if result.stdout.len() > 0 {
                Ok(String::from_utf8_lossy(&result.stdout).into_owned())
            } else {
                Err(format!("No version found for package {}", packagename))
            }
        }
        Err(e) => Err(format!(
            "Error while reading package version of {}: {}",
            packagename, e
        )),
    }
}

fn modify_file(
    filename: &str,
    pattern: &str,
    replacement: &str,
    build_dir: &str,
    append_if_not_exist: bool,
    sudo: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    //! Replace a row in filename containing the pattern with replacement.
    //! Set append_if_not_exist to 1 if the replacement should be added to the end of the file if the
    //! pattern wasn't found. Additionally, do nothing, if the search pattern already exists in the
    //! file
    //! The pattern needs to be given as regex
    //! be mindfull of special characters in the pattern, especially rust specifics
    //! $$      Match single dollar sign.

    let mut content: String = "".to_string();

    // if the file needs to be opened with sudo, it needs to be read with cat in linux
    if !sudo {
        content = read_to_string(filename)?;
    } else {
        let output = Command::new("sudo")
            .arg("cat")
            .arg(filename)
            .output()
            .expect("Failed to execute sudo command");

        if output.status.success() {
            content = String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_string();
        } else {
            let error_message = String::from_utf8_lossy(&output.stderr);
            eprintln!("Error reading file: {}", error_message);
        }
        // remove trailing empty lines from the file
    }

    let re = Regex::new(pattern)?;
    // check if the searched text exists and can be replaced:
    let search_content = re.is_match(&content);
    // check if the searched text already exists as a complete line
    let mut replacement_already_exist = false;
    for line in content.split("\n") {
        if replacement_already_exist {
            break;
        }
        let re = Regex::new(&format!("^{}$", replacement))?;
        replacement_already_exist = re.is_match(&line);
    }

    // try to replace the content
    let mut modified_content = re.replace_all(&content, replacement).to_string();

    if !search_content && append_if_not_exist {
        // the content didn't exist --> it couldn't be replaced and needs to be appended to the
        // file.
        modified_content = content;
        modified_content.push_str(&format!("\n{}", &replacement));
    }

    // write the modified string to the file. If the file needs sudo permissions, write with sudo
    // only perform, if the replacement wasn't already in the file as a complete line
    if !replacement_already_exist {
        let tmp_file_split: Vec<&str> = filename.rsplitn(2, "/").collect();
        let tmp_file = format!("{}/{}", build_dir, tmp_file_split.get(0).unwrap());
        let _ = File::create(&tmp_file);
        let _ = write(&tmp_file, modified_content);

        if !sudo {
            create_cmd_thread(vec![format!("cp {tmp_file} {filename}")], true);
        } else {
            create_cmd_thread(vec![format!("sudo cp {tmp_file} {filename}")], true);
        }
    }

    Ok(())
}

fn load_config_from_file(file_path: &str, args: &Args) -> Config {
    //! takes the path to the config file, parses the toml files and returns a config struct
    let content = read_to_string(file_path).expect("Failed to read TOML configfile");
    let mut configs: Config =
        toml::from_str(&content).expect("Errors in the TOML-structure of the configfile");

    // use pacconfig from args if available
    if args.pacconfig != "none" {
        configs.pacconfig = args.pacconfig.clone();
    }

    configs.pacconfig = resolve_home(configs.pacconfig);

    // if overlay-dir starts with ~ or $HOME, parse the directory
    configs.overlay_dir = resolve_home(configs.overlay_dir);

    // if patch-dir starts with ~ or $HOME, parse the directory
    configs.patch_dir = resolve_home(configs.patch_dir);

    // if overlay-dir starts with ~ or $HOME, parse the directory
    configs.mirrorlist = resolve_home(configs.mirrorlist);

    let mut local_repo_dir: String = String::new();

    if configs.local_repo.ends_with(".db.tar.zst") {
        configs.local_repo = resolve_home(configs.local_repo);

        let check_file_exists = Path::new(&configs.local_repo);
        if check_file_exists.is_file() {
            local_repo_dir = configs.local_repo.rsplit_once("/").unwrap().0.to_string();
        } else {
            // initiate, if anything other then no or n is defined
            if args.initiate != "no" && args.initiate != "n" {
                println!("Repository Db.tar.zst-file doesn't exist. It will be created");
                initiate_repo(&configs);
            } else {
                local_repo_dir = "none".to_string();
                println!("{}", "Repository Db.tar.zst-file doesn't exist --> no local builds are possible. To create the file restart with -i yes".red());
            }
        }
    } else {
        local_repo_dir = "none".to_string();
        println!(
            "{}",
            "No db.tar.zst-file for local repository specified --> no local builds are possible."
                .red()
        );
    }

    configs.local_repo = local_repo_dir;

    configs
}

fn resolve_home(old_path: String) -> String {
    //if path of config-file contains ~ or $HOME, parse to the real home dir
    let home_dir: String = home_dir().unwrap().display().to_string();

    let mut new_path: String = old_path;

    if new_path.trim().starts_with("~") {
        new_path = new_path.replace("~", &home_dir);
    }

    if new_path.trim().starts_with("$HOME") {
        new_path = new_path.replace("$HOME", &home_dir);
    }

    new_path
}

fn initiate_repo(config: &Config) {
    //! initiate nompac.
    //! Takes config struct
    //! Creates local repo according to the defined local_repo config option
    let local_repo_file = config.local_repo.split("/").last().unwrap().to_string();
    create_cmd_thread(vec![format!("repo-add {}", local_repo_file)], true);
}

fn initiate_pacmanconf(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    //! initiate nompac.
    //! Takes config struct
    //! Updates pacman.conf with configured mirrorlist and adds local repo

    // change mirrorlist to the one configured
    let _ = modify_file(
        &config.pacconfig,
        "Include.*mirrorlist",
        &format!("Include = {}", &config.mirrorlist),
        &config.build_dir,
        false,
        false,
    )?;

    // add local repository
    if config.local_repo != "none" {
        let contents = read_to_string(&config.pacconfig)?;
        let mut modified_content: String = String::new();
        let mut already_inserted = false;
        for line in contents.lines() {
            // insert local repo before the first defined repository found
            if !already_inserted
                & (line.ends_with("[core-testing]")
                    | line.ends_with("[core]")
                    | line.ends_with("[extra-testing]")
                    | line.ends_with("[extra]")
                    | line.ends_with("[multilib]"))
            {
                modified_content.push_str("[nomispaz]\n");
                modified_content.push_str("SigLevel = Optional TrustAll\n");
                modified_content.push_str(&format!("Server = file://{}\n\n", config.local_repo));
                modified_content.push_str(&format!("{}\n", line));
                already_inserted = true;
            } else {
                modified_content.push_str(&format!("{}\n", line));
            }
        }

        // Write new content to file
        write(&config.pacconfig, &modified_content)?;
    }
    Ok(())
}

fn run_commands_stdout(commands: Vec<&str>) -> Result<Output, std::io::Error> {
    //! takes vector of bash commands and executes the commands
    let joined_command = commands.join("; ");
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(joined_command).output()
}

// Function to spawn the bash command and send each line of output over the channel
fn run_command(
    tx: Sender<String>,
    commands: Vec<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let joined_command = commands.join("; ");
    // Run the Bash command
    let mut cmd = Command::new("bash")
        .arg("-c")
        .arg(joined_command)
        .stdout(Stdio::piped()) // Capture the output
        .spawn()?;

    // Check if we can capture the stdout
    if let Some(stdout) = cmd.stdout.take() {
        let reader = BufReader::new(stdout);

        // Send each line as it is available to the main thread
        for line in reader.lines() {
            let line = line.unwrap();
            tx.send(line)?;
        }
    }

    // Ensure the command completes
    cmd.wait()?;
    Ok(())
}

fn create_cmd_thread(command: Vec<String>, print: bool) {
    //! wrapper to create a new thread to run bash script, print the results and return errors if necessary
    //! takes a vector of commands and print as a boolean value (true --> print results of cmd)

    // Create a channel to communicate between threads
    let (tx, rx) = mpsc::channel();

    // Spawn a thread to run the Bash command asynchronously
    // clone of command necessary to print it on error
    let command_result = thread::spawn(move || match run_command(tx, command.clone()) {
        Ok(_) => print!("Successfully ran command {}", command.join("; ")),
        Err(e) => eprintln!("Failed to run command {}: {:?}", command.join("; "), e),
    });

    // Main thread: receive and print each line as it arrives
    // print only when print was set to true
    if print {
        for received_line in rx {
            println!("{received_line}");
        }
    }

    // make sure, that the thread is finished before continuing
    match command_result.join() {
        Ok(_) => println!(""),
        Err(e) => eprintln!("Main thread: Failed to join thread: {:?}", e),
    }
}

fn collect_package_lists(configs: &Config) -> (Vec<String>, Vec<String>) {
    //! returns lists for the packages to be removed or installed

    // get list of explicitely installed packages
    let mut package_list_installed: Vec<String> = String::from_utf8_lossy(
        &Command::new("bash")
            .arg("-c")
            .arg("pacman -Qe | cut -d' ' -f 1")
            .output()
            .map_err(|e| eprintln!("List of installed packages couldn't be collected: {e}"))
            .unwrap()
            .stdout,
    )
    .split("\n")
    .map(|s| s.to_string())
    .collect();

    // remove last item from list since the split function returns one additional empty row
    package_list_installed.pop();

    // create package list of packages that should be installed explicitely
    let mut package_list = configs.packages.clone();
    // sort list by package name
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

    return (packages_to_remove, packages_to_install);
}

fn perform_config_changes(configs: &Config) {
    // loop through all defined changes for config-files
    for entry in configs.configs.clone() {
        let file_path_resolved = resolve_home(entry.path.clone());

        // check if the file exists
        if !Path::new(&file_path_resolved).exists() {
            let _ = File::create(&file_path_resolved);
            println!("File {file_path_resolved} created.");
        }

        for config_entry in entry.clone() {
            // extract orig and changed value
            let orig_value = &config_entry.orig;
            let changed_value = &config_entry.changed;

            let _ = modify_file(
                &file_path_resolved,
                orig_value,
                changed_value,
                &configs.build_dir,
                true,
                true,
            );
        }
    }
}

fn collect_settings(file_path: &str) -> (Vec<String>, Vec<String>) {
    //! collect the packages and overlays defined in the given file and return them

    // first read the contents of the file into a toml value (at this point it is not known, what fields exist in the toml file)

    // Read the file contents into a string
    let contents =
        read_to_string(file_path).expect(&format!("Couldn't read config file: {}", file_path));

    // Parse the string into a `serde::Value`
    let toml_table: Table =
        toml::from_str(&contents).expect(&format!("Couldn't parse config file {}", file_path));

    let mut packages: Vec<String> = vec![];
    let mut overlays: Vec<String> = vec![];

    // collect the packages
    for entry in toml_table {
        if entry.0 == "packages" {
            packages = entry
                .1
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s.to_string().replace("\"", ""))
                .collect();
        }
        if entry.0 == "overlays" {
            overlays = entry
                .1
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s.to_string().replace("\"", ""))
                .collect();
        }
    }

    (packages, overlays)
}

fn cleanup_system() {
    let mut command: Vec<String> = vec![];
    // show failed daemons
    command.push("systemctl --failed".to_string());
    command.push("echo \"Search and remove orphaned packages\"".to_string());
    command.push("sudo pacman -Qdtq | sudo pacman -Rns -".to_string());
    command.push("echo \"cleanup the package cache (keep the last version)\"".to_string());
    command.push("sudo paccache -rk1".to_string());
    command.push("echo \"remove all uninstaled packages from the package cache\"".to_string());
    command.push("sudo paccache -ruk0".to_string());
    create_cmd_thread(command, true);
}

fn rebuild_grub() {
    let mut command: Vec<String> = vec![];
    command.push("sudo grub-install --target=x86_64-efi --efi-directory=/boot/efi".to_string());
    command.push("sudo grub-mkconfig -o /boot/grub/grub.cfg".to_string());
    create_cmd_thread(command, true);
}

fn main() {
    // define and read command line arguments
    let args = Args::parse();

    let mut path_to_config = args.config.clone();

    path_to_config = resolve_home(path_to_config);

    // Import basic settings from the config-file
    let mut configs = load_config_from_file(&path_to_config, &args);

    // collect settings from imported config-files defined in the original config file
    for file_name in configs.imports.clone() {
        let additional_settings: (Vec<String>, Vec<String>) =
            collect_settings(&resolve_home(file_name));
        configs.packages.extend(additional_settings.0);
        configs.overlays.extend(additional_settings.1);
    }

    // initiate pacman.conf if required
    if args.initiate != "no" && args.initiate != "n" {
        let _ = initiate_pacmanconf(&configs);
    }

    // get snapshot date
    let date: Vec<String>;

    let original_snapshot = configs.snapshot.clone();

    // if a snapshot was defined in the arguments, replace the one from the config file
    if args.snapshot == "none" {
        date = configs.snapshot.split('_').map(|s| s.to_string()).collect();
    } else {
        date = args.snapshot.split('_').map(|s| s.to_string()).collect();
    }

    // all settings are collected --> print the result for the user
    println!("{}", "Used settings:".blue());
    println!("Used config file: {}", path_to_config);
    println!("Local build directory: {}", configs.build_dir);
    println!("Local repository: {}", configs.local_repo);
    println!("Patch directory: {}", configs.patch_dir);
    println!("Overlay directory: {}", configs.overlay_dir);
    println!("pacman.conf location: {}", configs.pacconfig);
    println!("Snaphot date: {}_{}_{}", date[0], date[1], date[2]);

    //building custom packages and overlays
    if configs.local_repo != "none".to_string() {
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
                Err(e) => println!(
                    "{}",
                    format!(
                        "Package version in repository of package {} couldn't be determined: {}",
                        package, e
                    )
                    .red()
                ),
            }
            match get_installed_version(&package) {
                Ok(version) => package_version_installed = version,
                Err(e) => println!(
                    "{}",
                    format!(
                        "Package version of installed package {} couldn't be determined: {}",
                        package, e
                    )
                    .red()
                ),
            }

            //only procede if the package was updated upstream
            if package_version_installed.trim() != package_version_repo.trim() {
                let _ = get_current_tarball_from_repo(
                    &package,
                    &package_version_repo,
                    &format!(
                        "{}/{}-{}.tar.gz",
                        configs.build_dir, package, package_version_repo
                    ),
                );

                println!(
                    "{}/{}-{}.tar.gz",
                    &configs.build_dir, &package, &package_version_repo
                );
                let _ = extract_tgz(
                    &format!(
                        "{}/{}-{}.tar.gz",
                        &configs.build_dir, &package, &package_version_repo
                    ),
                    &format!("{}/src/", &configs.build_dir),
                );

                apply_patches(
                    &configs,
                    &configs.patches[0][package],
                    &package,
                    &package_version_repo,
                );

                build_package(&format!(
                    "{}/src/{}-{}/",
                    configs.build_dir, package, package_version_repo
                ));

                let _ = update_repository(&configs, &configs.local_repo, &package);

                cleanup(&configs);
            } else {
                println!(
                    "{}",
                    format!("Package {} already up to date.", package).green()
                );
            }
        }

        // build packages from overlays
        println!("{}", "\nBuilding packages from overlay".blue());

        for package in &configs.overlays {
            let mut package_version_overlay: String = String::new();
            let mut package_version_installed: String = String::new();

            match get_installed_version(&package) {
                Ok(version) => package_version_installed = version,
                Err(e) => println!(
                    "{}",
                    format!(
                        "Package version of installed package {} couldn't be determined: {}",
                        package, e
                    )
                    .red()
                ),
            }

            match get_version_from_overlay(&configs.overlay_dir, &package) {
                Ok(version) => package_version_overlay = version,
                Err(e) => println!(
                    "{}",
                    format!(
                        "Package version of package {} from overlay couldn't be determined: {}",
                        package, e
                    )
                    .red()
                ),
            }

            if package_version_installed.trim() != package_version_overlay.trim() {
                // first check if the package was alread build and is available in the local repo
                let package_version_from_repo: Vec<String> = String::from_utf8_lossy(
                    &Command::new("bash")
                        .arg("-c")
                        .arg(format!(
                            "pacman -Ss | grep \"\\<{}\\>\" | cut -d' ' -f 2",
                            &package
                        ))
                        .output()
                        .map_err(|e| eprintln!("Version of package couldn't be determined: {e}"))
                        .unwrap()
                        .stdout,
                )
                .split("\n")
                .map(|s| s.to_string())
                .collect();

                if Some(package_version_from_repo) == None {
                    // there is no package in the repository

                    // copy necessary files from overlay to build directory
                    for entry in WalkDir::new(&format!("{}/{}", &configs.overlay_dir, package))
                        .into_iter()
                        .filter_map(|entry| entry.ok())
                    {
                        if entry.path().is_file() {
                            let _ = std::fs::create_dir_all(format!(
                                "{}/src/{}/",
                                configs.build_dir, package
                            ));
                            let _ = copy(
                                entry.path(),
                                format!(
                                    "{}/src/{}/{}",
                                    configs.build_dir,
                                    package,
                                    entry.file_name().to_str().unwrap()
                                ),
                            );
                            println!("{}/src/{}/", configs.build_dir, package);
                        }
                    }

                    // build the package
                    build_package(&format!("{}/src/{}/", configs.build_dir, package));

                    let _ = update_repository(&configs, &configs.local_repo, &package);

                    //create_cmd_thread(
                    //    vec![format!(
                    //        "sudo pacman -Sy {} --config {}",
                    //        package, &configs.pacconfig
                    //    )],
                    //    true,
                    //);

                    cleanup(&configs);
                }
            } else {
                println!(
                    "{}",
                    format!("Package {} already up to date.", package).green()
                );
            }
        }
    }

    // perform system update
    if date[0] != "none" {
        // update snapshot that will be used for the update
        let _ = modify_file(
            &format!("{}/mirrorlist", path_to_config.rsplit_once("/").unwrap().0),
            ".*archive.archlinux.org.*",
            &format!(
                "Server = https://archive.archlinux.org/repos/{}/{}/{}/$$repo/os/$$arch",
                date[0], date[1], date[2]
            ),
            &configs.build_dir,
            true,
            false,
        );

        let (packages_to_remove, packages_to_install) = collect_package_lists(&configs);

        // only perform if packages have to be removed
        if packages_to_remove.len() > 0 {
            println!(
                "{}",
                "Removing the following packages since they don't exist in the config file:".red()
            );
            let mut command: Vec<String> = Vec::new();
            let mut package_list: String = String::new();
            for package in packages_to_remove {
                package_list.push_str(" ");
                package_list.push_str(&package);
            }
            command.push(format!("sudo pacman -Rsn {}", package_list));
            println!("{}", format!("{}", package_list).red());

            create_cmd_thread(command, true);
        }

        // only perform if packages have to be installed
        if packages_to_install.len() > 0 {
            println!(
                "{}",
                "Installing the following packages and starting update:".blue()
            );
            let mut command: Vec<String> = Vec::new();
            let mut package_list: String = String::new();
            for package in packages_to_install {
                package_list.push_str(" ");
                package_list.push_str(&package);
            }

            command.push(format!(
                "sudo pacman -Syu {} --config {}",
                package_list, configs.pacconfig
            ));
            println!("{}", format!("{}", package_list).blue());
            create_cmd_thread(command, true);

            // after running the update, check for changed config files
            let _ = Command::new("bash")
                .arg("-c")
                .arg("sudo DIFFPROG='nvim -d' pacdiff")
                .status();
        } else {
            println!("{}", "Starting system update.\n".blue());
            let mut command: Vec<String> = Vec::new();

            command.push(format!("sudo pacman -Syu --config {}", configs.pacconfig));
            create_cmd_thread(command, true);

            // after running the update, check for changed config files
            let _ = Command::new("bash")
                .arg("-c")
                .arg("sudo DIFFPROG='nvim -d' pacdiff")
                .status();
        }
    }

    if !configs.configs.is_empty() {
        // TODO
        perform_config_changes(&configs);
    }

    // remove old and orphaned packages, check for failed daemons
    cleanup_system();

    
    // update config file with the date that was given as snapshot
    if args.snapshot != "none" {
        let _ = modify_file(&resolve_home(args.config), &format!("snapshot = \"{}\"",original_snapshot), &format!("snapshot = \"{}\"", args.snapshot), &configs.build_dir, true, false);
    }

    // rebuild grub in case there was a breaking change
    println!("\n\nReinstall grub and generate grub.cfg? Should be done if grub update had breaking changes (y/N)");

    let mut execute_grub_rebuild = String::new();
    stdin()
        .read_line(&mut execute_grub_rebuild)
        .expect("Failed to read line");

    match execute_grub_rebuild.to_lowercase().trim() {
        "y" => {
            // rebuilding grub (reinstall and generate)
            println!("\n\nRebuilding grub.");
            rebuild_grub();
        }
        _ => {
            println!("");
        }
    }
}

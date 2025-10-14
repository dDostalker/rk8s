use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::Result;
use anyhow::anyhow;
use clap::{Arg, ArgAction, Subcommand};
use serde::Deserialize;
use serde::Serialize;
use std::fmt::Write as _;
use std::io::Write;
use tabwriter::TabWriter;

use crate::commands::utils::parse_key_val;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeMetadata {
    pub name: String,
    pub driver: String,
    pub mountpoint: PathBuf,
    pub created_at: String,
    pub labels: HashMap<String, String>,
    pub options: HashMap<String, String>,
    pub scope: String, // "local" or "global"
    pub status: HashMap<String, String>,
}

#[derive(Subcommand)]
pub enum VolumeCommand {
    #[command(about = "Create a volume")]
    Create {
        #[arg(value_name = "VOLUME_NAME")]
        name: String,
        #[arg(long, short = 'd')]
        driver: Option<String>,
        #[arg(long, short = 'o', value_parser=parse_key_val)]
        opt: Option<Vec<(String, String)>>,
    },

    #[command(about = "Remove one or more volumes")]
    Rm {
        volumes: Vec<String>,
        #[arg(long, short = 'f', action=ArgAction::SetTrue)]
        force: bool,
    },

    #[command(about = "List volumes")]
    Ls {
        #[arg(long, short = 'q', action=ArgAction::SetTrue)]
        quiet: bool,
    },

    #[command(about = "Display detailed information on one or more volumes")]
    Inspect { name: Vec<String> },

    #[command(about = "Remove all unused local volumes")]
    Prune {
        #[arg(long, short = 'f', action=ArgAction::SetTrue)]
        force: bool,
    },
}

pub struct VolumeManager {
    volume_root: PathBuf,   // /var/lib/rkl/volumes
    metadata_path: PathBuf, // /var/lib/rkl/volumes/metadata.json
    volumes: HashMap<String, VolumeMetadata>,
}

impl VolumeManager {
    pub fn new() -> Result<Self> {
        let volume_root = PathBuf::from("/var/lib/rkl/volumes");
        let metadata_path = volume_root.join("metadata.json");

        fs::create_dir_all(&volume_root)?;

        let volumes = if metadata_path.exists() {
            Self::load_metadata(&metadata_path)?
        } else {
            HashMap::new()
        };

        Ok(Self {
            volume_root,
            metadata_path,
            volumes,
        })
    }

    pub fn create(
        &mut self,
        name: String,
        driver: Option<String>,
        opts: HashMap<String, String>,
    ) -> Result<VolumeMetadata> {
        if self.volumes.contains_key(&name) {
            return Err(anyhow!("volume {} already exists", name));
        }

        let driver = driver.unwrap_or_else(|| "local".to_string());
        let mountpoint = self.volume_root.join(&name).join("_data");

        fs::create_dir_all(&mountpoint)?;

        let metadata = VolumeMetadata {
            name: name.clone(),
            driver,
            mountpoint,
            created_at: chrono::Utc::now().to_rfc3339(),
            labels: HashMap::new(),
            options: opts,
            scope: "local".to_string(),
            status: HashMap::new(),
        };

        self.volumes.insert(name, metadata.clone());
        self.save_metadata()?;

        Ok(metadata)
    }

    pub fn remove(&mut self, name: &str, force: bool) -> Result<()> {
        let volume = self
            .volumes
            .get(name)
            .ok_or_else(|| anyhow!("volume {} not found", name))?;

        if !force && self.is_volume_in_use(name)? {
            return Err(anyhow!("volume {} is in use", name));
        }

        fs::remove_dir_all(&volume.mountpoint.parent().unwrap())?;
        self.volumes.remove(name);
        self.save_metadata()?;

        Ok(())
    }

    pub fn list(&self) -> Vec<&VolumeMetadata> {
        self.volumes.values().collect()
    }

    pub fn inspect(&self, name: &str) -> Result<&VolumeMetadata> {
        self.volumes
            .get(name)
            .ok_or_else(|| anyhow!("volume {} not found", name))
    }

    // docker volume prune
    pub fn prune(&mut self) -> Result<Vec<String>> {
        let mut removed = Vec::new();
        let names: Vec<String> = self.volumes.keys().cloned().collect();

        for name in names {
            if !self.is_volume_in_use(&name)? {
                self.remove(&name, true)?;
                removed.push(name);
            }
        }

        Ok(removed)
    }

    /// scan all the container's state json
    /// check if there is container refer this volume
    fn is_volume_in_use(&self, name: &str) -> Result<bool> {
        Ok(false)
    }

    fn save_metadata(&self) -> Result<()> {
        let json = serde_json::to_string_pretty(&self.volumes)?;
        fs::write(&self.metadata_path, json)?;
        Ok(())
    }

    fn load_metadata(path: &Path) -> Result<HashMap<String, VolumeMetadata>> {
        let content = fs::read_to_string(path)?;
        Ok(serde_json::from_str(&content)?)
    }

    // ========Command entrypoints========
    fn remove_(&mut self, names: Vec<String>, force: bool) -> Result<()> {
        for name in names {
            self.remove(name.as_str(), force)?;
            println!("{name} removed");
        }
        Ok(())
    }

    fn create_(
        &mut self,
        name: String,
        driver: Option<String>,
        opts: HashMap<String, String>,
    ) -> Result<()> {
        let metadata = self.create(name, driver, opts)?;
        println!("{}", metadata.name);
        Ok(())
    }

    fn ls(&self, quiet: bool) -> Result<()> {
        let volumes = self.list();
        let mut content = String::new();
        for v in volumes {
            if !quiet {
                let _ = writeln!(content, "{}\t{}", v.driver, v.name);
            } else {
                let _ = writeln!(content, "{}", v.name);
            }
        }

        let mut tab_writer = TabWriter::new(io::stdout());
        if !quiet {
            writeln!(&mut tab_writer, "DRIVER\tVOLUME NAME")?;
        } else {
            writeln!(&mut tab_writer, "VOLUME NAME")?;
        }
        write!(&mut tab_writer, "{content}")?;
        tab_writer.flush()?;

        Ok(())
    }
    fn inspect_(&self, names: Vec<String>) -> Result<()> {
        for name in names {
            let meta = self.inspect(name.as_str())?;
            let meta_str = serde_json::to_string_pretty(meta)?;
            println!("{meta_str}");
        }
        Ok(())
    }
    fn prune_(&self) -> Result<()> {
        Ok(())
    }
}

pub fn volume_execute(cmd: VolumeCommand) -> Result<()> {
    let mut v_manager = VolumeManager::new()?;
    match cmd {
        VolumeCommand::Create { name, driver, opt } => {
            v_manager.create_(name, driver, HashMap::new())
        }
        VolumeCommand::Rm { volumes, force } => v_manager.remove_(volumes, force),
        VolumeCommand::Ls { quiet } => v_manager.ls(quiet),
        VolumeCommand::Inspect { name } => v_manager.inspect_(name),
        VolumeCommand::Prune { force } => v_manager.prune_(),
    }
}

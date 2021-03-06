use action;
use bitmap;
use config;
use controller;
use displaymanager::{DisplayManager, Position, Widget, Window, WindowId};
use error::Result;
use fs;
use input;
use lvm;
use newdrive;
use options;
use usb;
use std::sync::{Arc, Mutex};
use render;
use state;
use stats;
use utils;
use vdrive;
use version;
use wifi;

pub struct PIso {
    config: config::Config,
    pub drives: Vec<vdrive::VirtualDrive>,
    newdrive: newdrive::NewDrive,
    stats: stats::Stats,
    usb: Arc<Mutex<usb::UsbGadget>>,
    vg: lvm::VolumeGroup,
    window: WindowId,
    wifi: wifi::WifiMenu,
    options: options::Options,
    version: version::PiVersion,
}

impl PIso {
    pub fn new(
        disp: &mut DisplayManager,
        usb: Arc<Mutex<usb::UsbGadget>>,
        config: &config::Config,
    ) -> Result<PIso> {
        let window = disp.add_child(Position::Fixed(0, 0))?;

        let vg = lvm::VolumeGroup::from_path("/dev/VolGroup00")?;
        let drives = Self::build_drives_from_vg(disp, &vg, &usb, config)?;
        let ndrive = newdrive::NewDrive::new(disp, usb.clone(), vg.clone(), config.clone())?;
        let stats = stats::Stats::new(disp, vg.clone())?;
        let wifi = wifi::WifiMenu::new(disp, config)?;
        let options = options::Options::new(disp, &vg, config)?;

        if drives.len() > 0 {
            // Focus the first drive
            drives.iter().next().map(|drive| {
                disp.shift_focus(drive as &Widget);
            });
        } else {
            disp.shift_focus(&ndrive);
        }

        // Add the user account if it doesn't exit and ensure the password
        // is what is expected. This can take a little while, so do this
        // async.
        PIso::configure_user(config)?;

        Ok(PIso {
            config: config.clone(),
            drives: drives,
            newdrive: ndrive,
            usb: usb,
            vg: vg,
            window: window,
            stats: stats,
            wifi: wifi,
            options: options,
            version: version::read_version()?,
        })
    }

    fn configure_user(config: &config::Config) -> Result<()> {
        utils::run_check_output(
            "/opt/piso_scripts/add_user.sh",
            &[&config.user.name, &config.user.password],
        )?;
        fs::create_dir("/user-mnt")?;
        utils::run_check_output(
            "bindfs",
            &[
                "--multithreaded",
                "-u",
                &config.user.name,
                "/mnt",
                "/user-mnt",
            ],
        )?;
        Ok(())
    }

    fn build_drives_from_vg(
        disp: &mut DisplayManager,
        vg: &lvm::VolumeGroup,
        usb: &Arc<Mutex<usb::UsbGadget>>,
        config: &config::Config,
    ) -> Result<Vec<vdrive::VirtualDrive>> {
        let mut drives: Vec<vdrive::VirtualDrive> = vec![];
        for vol in vg.volumes()?.into_iter() {
            drives.push(vdrive::VirtualDrive::new(disp, usb.clone(), vol, config)?)
        }
        Ok(drives)
    }

    fn add_drive<'a, 'b>(
        &'a mut self,
        disp: &'b mut DisplayManager,
        volume: lvm::LogicalVolume,
    ) -> Result<&'a mut vdrive::VirtualDrive> {
        let mut vdrive = vdrive::VirtualDrive::new(disp, self.usb.clone(), volume, &self.config)?;
        vdrive.mount_internal(disp)?;
        self.drives.push(vdrive);

        Ok(self.drives
            .last_mut()
            .expect("vdrive was somehow empty after push"))
    }

    fn share_drive(drive: &mut vdrive::VirtualDrive, remove: bool) -> Result<Vec<action::Action>> {
        if !version::read_version()?.has_wifi() {
            return Ok(vec![]);
        }
        match drive.state {
            vdrive::MountState::Unmounted | vdrive::MountState::External(_) => {
                if remove {
                    Ok(vec![])
                } else {
                    Err("Cannot share drive when not mounted internal".into())
                }
            }
            vdrive::MountState::Internal(ref info) => Ok(info.part_mount_paths
                .iter()
                .map(|path| {
                    let name = path.file_name()
                        .expect("Partition has no name")
                        .to_string_lossy()
                        .into_owned();
                    if remove {
                        action::Action::SmbRemoveShare(name)
                    } else {
                        action::Action::SmbSharePartition(name)
                    }
                })
                .collect()),
        }
    }
}

impl render::Render for PIso {
    fn render(&self, _manager: &DisplayManager, _: &Window) -> Result<bitmap::Bitmap> {
        Ok(bitmap::Bitmap::new(0, 0))
    }
}

impl input::Input for PIso {
    fn on_event(&mut self, event: &controller::Event) -> Result<(bool, Vec<action::Action>)> {
        match event {
            _ => Ok((false, vec![])),
        }
    }

    fn do_action(
        &mut self,
        disp: &mut DisplayManager,
        action: &action::Action,
    ) -> Result<(bool, Vec<action::Action>)> {
        match *action {
            action::Action::FlipDisplay => {
                disp.display.flip_display();
                Ok((true, vec![]))
            }
            action::Action::CreateDrive(ref volume) => {
                let drive = self.add_drive(disp, volume.clone())?;
                let actions = PIso::share_drive(drive, false)?;
                Ok((true, actions))
            }
            action::Action::SnapshotDrive(ref name) => {
                let report = self.vg.snapshot_volume(name)?;
                let drive = self.add_drive(disp, report)?;
                let actions = PIso::share_drive(drive, false)?;
                Ok((true, actions))
            }
            action::Action::DeleteDrive(ref name) => {
                let mut actions = vec![];
                if let Some(ref mut drive) =
                    self.drives.iter_mut().find(|drive| drive.name() == name)
                {
                    actions = PIso::share_drive(drive, true)?;
                    drive.unmount()?;
                }
                self.drives.retain(|drive| drive.name() != name);
                self.vg.delete_volume(&name)?;
                Ok((true, actions))
            }
            _ => Ok((false, vec![])),
        }
    }
}

impl state::State for PIso {}

impl Widget for PIso {
    fn mut_children(&mut self) -> Vec<&mut Widget> {
        let mut ordered_children = self.drives
                .iter_mut()
                .collect::<Vec<&mut vdrive::VirtualDrive>>();

        match self.config.ui.sort_drives {
            Some(true) => ordered_children.sort_by(|drive1, drive2| {
                drive1.volume.name.cmp(&drive2.volume.name)
            }),
            _ => ()
        }

        let mut children = ordered_children
            .into_iter()
            .map(|vdrive| vdrive as &mut Widget)
            .collect::<Vec<&mut Widget>>();

        children.push(&mut self.newdrive as &mut Widget);
        if self.version.has_wifi() {
            children.push(&mut self.wifi as &mut Widget);
        }
        children.push(&mut self.options as &mut Widget);
        children.push(&mut self.stats as &mut Widget);
        children
    }

    fn children(&self) -> Vec<&Widget> {
        let mut ordered_children = self.drives
            .iter()
            .collect::<Vec<&vdrive::VirtualDrive>>();

        match self.config.ui.sort_drives {
            Some(true) => ordered_children.sort_by(|drive1, drive2| {
                utils::translate_drive_name(&drive1.volume.name, &self.config).cmp(
                    &utils::translate_drive_name(&drive2.volume.name, &self.config))
            }),
            _ => ()
        }

        let mut children = ordered_children
            .into_iter()
            .map(|vdrive| vdrive as &Widget)
            .collect::<Vec<&Widget>>();

        children.push(&self.newdrive as &Widget);
        if self.version.has_wifi() {
            children.push(&self.wifi as &Widget);
        }
        children.push(&self.options as &Widget);
        children.push(&self.stats as &Widget);
        children
    }

    fn windowid(&self) -> WindowId {
        self.window
    }
}

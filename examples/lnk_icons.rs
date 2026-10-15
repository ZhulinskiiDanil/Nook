//! Debug: prints IconLocation and target of every .lnk on the user Desktop. Read-only.

use windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile, STGM_READ};
use windows::Win32::UI::Shell::{IShellLinkW, SLGP_RAWPATH, ShellLink};
use windows::core::{HSTRING, Interface};

fn main() -> anyhow::Result<()> {
    let _com = nook::platform::ComApartment::sta()?;
    let desk = nook::paths::Desktops::resolve()?.user;
    for e in std::fs::read_dir(&desk)?.flatten() {
        let p = e.path();
        if !p.extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk")) {
            continue;
        }
        unsafe {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            let pf: IPersistFile = link.cast()?;
            pf.Load(&HSTRING::from(p.as_os_str()), STGM_READ)?;
            let mut icon = vec![0u16; 1024];
            let mut idx = 0i32;
            let _ = link.GetIconLocation(&mut icon, &mut idx);
            let mut target = vec![0u16; 1024];
            let mut fd = WIN32_FIND_DATAW::default();
            let _ = link.GetPath(&mut target, &mut fd, SLGP_RAWPATH.0 as u32);
            let s = |v: &[u16]| String::from_utf16_lossy(&v[..v.iter().position(|&c| c == 0).unwrap_or(v.len())]);
            let name = p.file_name().unwrap().to_string_lossy().replace('\u{2800}', "⠿");
            println!("{name:<28} icon=[{}],{idx}  target=[{}]", s(&icon), s(&target));
        }
    }
    Ok(())
}

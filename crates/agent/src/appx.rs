//! Windows Store apps' logos, from their package: the app's entry in
//! AppxManifest.xml names a logo ("Assets\AppList.png"), and the package
//! holds it in several files, one per size and look
//! ("AppList.targetsize-32_altform-unplated.png", "AppList.scale-200.png").
//! The taskbar shows the "unplated" ones (in colour, no tile behind).
//!
//! Pure functions; the Windows side reads the files (coh_win.rs,
//! apps_win.rs).

/// The logo (a path in the package, '\' or '/' between names) of the app
/// whose `Id` is `id`, else whose `Executable` is `exe` (relative to the
/// package, any case), else of the first app. Its small square logo
/// (`Square44x44Logo`, the list and taskbar one), else the big one,
/// else the package's.
pub fn logo(manifest: &str, id: Option<&str>, exe: Option<&str>) -> Option<String> {
    let doc = roxmltree::Document::parse(manifest).ok()?;
    let apps: Vec<roxmltree::Node> = doc.descendants().filter(|n| n.tag_name().name() == "Application").collect();
    let norm = |s: &str| s.replace('/', "\\").to_lowercase();
    let app = id
        .and_then(|id| apps.iter().find(|a| a.attribute("Id") == Some(id)))
        .or_else(|| exe.and_then(|e| apps.iter().find(|a| a.attribute("Executable").is_some_and(|x| norm(x) == norm(e)))))
        .or(apps.first());
    let from_app = app.and_then(|a| {
        let v = a.descendants().find(|n| n.tag_name().name() == "VisualElements")?;
        v.attribute("Square44x44Logo").or(v.attribute("Square150x150Logo")).map(str::to_string)
    });
    from_app.or_else(|| {
        let p = doc.descendants().find(|n| n.tag_name().name() == "Properties")?;
        p.children().find(|n| n.tag_name().name() == "Logo")?.text().map(|t| t.trim().to_string())
    })
}

/// Which file of a logo to show at `size` pixels, among the names in its
/// folder. `name`: the logo's file name as the manifest gives it
/// ("AppList.png"); `base`: its size at scale 100 (44 for the small
/// square logo). The unplated ones at a target size first, then the
/// other target sizes, then the scaled ones; of a kind, the smallest at
/// least `size`, else the biggest. High contrast and light-theme ones
/// never.
pub fn pick(name: &str, names: &[String], base: u32, size: u32) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    let (stem, ext) = (stem.to_lowercase(), ext.to_lowercase());
    let mut best: Option<((u32, u32, u32), &String)> = None;
    for n in names {
        let l = n.to_lowercase();
        let Some(rest) = l.strip_prefix(&stem).and_then(|r| r.strip_suffix(&format!(".{ext}"))) else { continue };
        let quals: Vec<&str> = match rest.strip_prefix('.') {
            Some(q) => q.split('_').collect(),
            None if rest.is_empty() => Vec::new(),
            None => continue,
        };
        let mut px = base;
        let mut kind = 2;
        let mut unplated = false;
        let mut skip = false;
        for q in &quals {
            if let Some(v) = q.strip_prefix("targetsize-") {
                px = v.parse().unwrap_or(0);
                kind = kind.min(1);
            } else if let Some(v) = q.strip_prefix("scale-") {
                px = base * v.parse::<u32>().unwrap_or(0) / 100;
            } else if *q == "altform-unplated" {
                unplated = true;
            } else if q.starts_with("contrast-") || q.starts_with("altform-") {
                skip = true;
            }
        }
        if skip || px == 0 {
            continue;
        }
        if unplated && kind == 1 {
            kind = 0;
        }
        // Too small ones after all big enough ones, the biggest first.
        let fit = if px >= size { (0, px) } else { (1, u32::MAX - px) };
        let k = (kind, fit.0, fit.1);
        if best.is_none_or(|(b, _)| k < b) {
            best = Some((k, n));
        }
    }
    best.map(|(_, n)| n.clone())
}

/// A logo path split into its folder and file name.
pub fn split(logo: &str) -> (String, String) {
    let l = logo.replace('/', "\\");
    match l.rsplit_once('\\') {
        Some((d, f)) => (d.to_string(), f.to_string()),
        None => (String::new(), l),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10" xmlns:uap="http://schemas.microsoft.com/appx/manifest/uap/windows10">
  <Properties><DisplayName>ms-resource:AppName</DisplayName><Logo>Assets\StoreLogo.png</Logo></Properties>
  <Applications>
    <Application Id="App" Executable="CalculatorApp.exe" EntryPoint="Windows.FullTrustApplication">
      <uap:VisualElements DisplayName="ms-resource:AppName" Square150x150Logo="Assets\CalculatorMedTile.png" Square44x44Logo="Assets\CalculatorAppList.png" BackgroundColor="transparent"/>
    </Application>
    <Application Id="Other" Executable="Sub\Other.exe">
      <uap:VisualElements Square150x150Logo="Assets/OtherTile.png"/>
    </Application>
  </Applications>
</Package>"#;

    #[test]
    fn the_apps_logo_is_found() {
        assert_eq!(logo(MANIFEST, None, Some("calculatorapp.exe")).as_deref(), Some(r"Assets\CalculatorAppList.png"));
        assert_eq!(logo(MANIFEST, None, Some("sub/other.exe")).as_deref(), Some("Assets/OtherTile.png"));
        assert_eq!(logo(MANIFEST, Some("Other"), None).as_deref(), Some("Assets/OtherTile.png"));
        assert_eq!(logo(MANIFEST, None, Some("nope.exe")).as_deref(), Some(r"Assets\CalculatorAppList.png"), "the first app");
        let bare = r#"<Package><Properties><Logo> Assets\StoreLogo.png </Logo></Properties></Package>"#;
        assert_eq!(logo(bare, None, None).as_deref(), Some(r"Assets\StoreLogo.png"));
        assert_eq!(logo("<not xml", None, None), None);
        // No DTDs (entities could blow up).
        assert_eq!(logo(r#"<!DOCTYPE x [<!ENTITY a "aaaa">]><Package/>"#, None, None), None);
        assert_eq!(split(r"Assets\Sub\A.png"), (r"Assets\Sub".to_string(), "A.png".to_string()));
        assert_eq!(split("A.png"), (String::new(), "A.png".to_string()));
    }

    #[test]
    fn the_best_file_is_picked() {
        let names: Vec<String> = [
            "CalculatorAppList.scale-100.png",
            "CalculatorAppList.scale-200.png",
            "CalculatorAppList.targetsize-16.png",
            "CalculatorAppList.targetsize-32.png",
            "CalculatorAppList.targetsize-24_altform-unplated.png",
            "CalculatorAppList.targetsize-48_altform-unplated.png",
            "CalculatorAppList.targetsize-32_altform-lightunplated.png",
            "CalculatorAppList.contrast-black_targetsize-32.png",
            "CalculatorMedTile.scale-100.png",
            "Other.png",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let p = |n: &[String]| pick("CalculatorAppList.png", n, 44, 32);
        assert_eq!(p(&names).as_deref(), Some("CalculatorAppList.targetsize-48_altform-unplated.png"));
        let no_unplated: Vec<String> = names.iter().filter(|n| !n.contains("unplated")).cloned().collect();
        assert_eq!(p(&no_unplated).as_deref(), Some("CalculatorAppList.targetsize-32.png"));
        let scaled: Vec<String> = names.iter().filter(|n| n.contains("scale-")).cloned().collect();
        assert_eq!(p(&scaled).as_deref(), Some("CalculatorAppList.scale-100.png"), "44 px is enough");
        let small = vec!["CalculatorAppList.targetsize-16.png".to_string(), "CalculatorAppList.targetsize-24.png".to_string()];
        assert_eq!(p(&small).as_deref(), Some("CalculatorAppList.targetsize-24.png"), "the biggest of too small ones");
        assert_eq!(pick("x.png", &["x.png".to_string()], 44, 32).as_deref(), Some("x.png"));
        assert_eq!(pick("x.png", &["xy.png".to_string()], 44, 32), None);
        assert_eq!(pick("noext", &names, 44, 32), None);
    }
}

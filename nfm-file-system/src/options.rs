//! Filesystem picker configuration shared by presentation backends.
#[derive(Clone, Debug)]
pub struct FileSystemPickerOptions {
    pub roots: Vec<String>,
    pub max_depth: i32,
    pub directories_only: bool,
    pub files_only: bool,
    pub search_string: Option<String>,
    pub preview_visible: bool,
    /// None preserves bat's configuration and BAT_THEME environment setting.
    pub bat_theme: Option<String>,
}

impl Default for FileSystemPickerOptions {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            max_depth: i32::MAX,
            directories_only: false,
            files_only: false,
            search_string: None,
            preview_visible: true,
            bat_theme: None,
        }
    }
}

/// Return a single root's parent, or the supplied drive list at a root.
pub fn parent_roots(roots: &[String], drive_roots: Vec<String>) -> Vec<String> {
    if roots.len() == 1 {
        if let Some(parent) = std::path::Path::new(&roots[0])
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
        {
            return vec![parent.to_string_lossy().into_owned()];
        }
    }
    drive_roots
}
impl FileSystemPickerOptions {
    /// Enter a subtree, clearing the query and initial depth/type restrictions.
    pub fn navigate_to(&self, roots: Vec<String>, preview_visible: bool) -> Self {
        Self {
            roots,
            preview_visible,
            bat_theme: self.bat_theme.clone(),
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parent_navigation_returns_parent_or_drives() {
        let drives = vec![r"C:\".into(), r"D:\".into()];
        assert_eq!(
            parent_roots(&[r"C:\work\child".into()], drives.clone()),
            vec![r"C:\work".to_owned()]
        );
        assert_eq!(parent_roots(&[r"C:\".into()], drives.clone()), drives);
        assert_eq!(parent_roots(&drives, drives.clone()), drives);
    }
    #[test]
    fn subtree_navigation_resets_filters_and_preserves_live_preview() {
        let options = FileSystemPickerOptions {
            max_depth: 2,
            files_only: true,
            search_string: Some("query".into()),
            bat_theme: Some("gruvbox-dark".into()),
            ..Default::default()
        };
        let next = options.navigate_to(vec!["child".into()], false);
        assert_eq!(next.roots, vec!["child".to_owned()]);
        assert_eq!(next.max_depth, i32::MAX);
        assert!(!next.files_only && !next.directories_only && !next.preview_visible);
        assert!(next.search_string.is_none());
        assert_eq!(next.bat_theme, options.bat_theme);
    }
}

//! The install form. Every field starts out with what the AppImage itself
//! declared, so only the wrong ones need fixing.

use std::path::{Path, PathBuf};

use appimg_core::install::{self, IconChoice, InstallRequest};
use appimg_core::metadata::AppImageInfo;
use appimg_core::{update, MAIN_CATEGORIES};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Comment,
    Categories,
    Args,
    Terminal,
    Icon,
    UpdateSource,
}

impl Field {
    pub const ALL: [Field; 7] = [
        Field::Name,
        Field::Comment,
        Field::Categories,
        Field::Args,
        Field::Terminal,
        Field::Icon,
        Field::UpdateSource,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Field::Name => "Name",
            Field::Comment => "Comment",
            Field::Categories => "Categories",
            Field::Args => "Arguments",
            Field::Terminal => "Terminal",
            Field::Icon => "Icon",
            Field::UpdateSource => "Update from",
        }
    }

    pub fn next(self) -> Field {
        let index = Field::ALL.iter().position(|f| *f == self).unwrap_or(0);
        Field::ALL[(index + 1) % Field::ALL.len()]
    }

    pub fn previous(self) -> Field {
        let index = Field::ALL.iter().position(|f| *f == self).unwrap_or(0);
        Field::ALL[(index + Field::ALL.len() - 1) % Field::ALL.len()]
    }
}

pub struct InstallForm {
    /// Kept alive because the extracted tree lives in it.
    pub info: AppImageInfo,
    pub request: InstallRequest,
    pub field: Field,
    pub category_cursor: usize,
    pub args_text: String,
    pub icon_text: String,
    pub update_source_text: String,
    /// Whether the update source was prefilled from the AppStream metadata.
    pub suggested: bool,
}

impl InstallForm {
    pub fn new(source: &Path, origin: &str, info: AppImageInfo) -> Self {
        let request = InstallRequest::from_info(source, origin, &info);
        let category_cursor =
            request.categories.first().and_then(|first| index_of(first)).unwrap_or(0);
        // What the AppStream metadata suggests is prefilled, in plain sight
        // and in the preview, so it is confirmed along with everything else.
        let suggestion = install::suggested_update_source(&request, &info);
        let suggested = suggestion.is_some();
        let update_source_text =
            suggestion.or_else(|| request.update_source.clone()).unwrap_or_default();

        Self {
            info,
            request,
            field: Field::Name,
            category_cursor,
            args_text: String::new(),
            icon_text: String::new(),
            update_source_text,
            suggested,
        }
    }

    pub fn source(&self) -> &Path {
        &self.request.source
    }

    pub fn is_selected(&self, category: &str) -> bool {
        self.request.categories.iter().any(|c| c == category)
    }

    pub fn toggle_category(&mut self) {
        let Some(category) = MAIN_CATEGORIES.get(self.category_cursor) else {
            return;
        };
        match self.request.categories.iter().position(|c| c == category) {
            Some(index) => {
                self.request.categories.remove(index);
            }
            None => self.request.categories.push((*category).to_string()),
        }
    }

    pub fn move_category(&mut self, delta: isize) {
        let last = MAIN_CATEGORIES.len() - 1;
        self.category_cursor = match delta {
            delta if delta < 0 => self.category_cursor.saturating_sub(delta.unsigned_abs()),
            delta => (self.category_cursor + delta as usize).min(last),
        };
    }

    /// The text buffer the current field edits, if it edits one.
    pub fn text_mut(&mut self) -> Option<&mut String> {
        match self.field {
            Field::Name => Some(&mut self.request.name),
            Field::Comment => Some(self.request.comment.get_or_insert_with(String::new)),
            Field::Args => Some(&mut self.args_text),
            Field::Icon => Some(&mut self.icon_text),
            Field::UpdateSource => Some(&mut self.update_source_text),
            Field::Categories | Field::Terminal => None,
        }
    }

    pub fn text(&self, field: Field) -> String {
        match field {
            Field::Name => self.request.name.clone(),
            Field::Comment => self.request.comment.clone().unwrap_or_default(),
            Field::Args => self.args_text.clone(),
            Field::Icon => self.icon_text.clone(),
            Field::UpdateSource => self.update_source_text.clone(),
            Field::Terminal => {
                if self.request.terminal {
                    "yes".to_string()
                } else {
                    "no".to_string()
                }
            }
            Field::Categories => self.request.categories.join(", "),
        }
    }

    /// Folds the edited text back into the request. Returns what is wrong
    /// with it, if anything.
    pub fn finish(&mut self) -> Result<(), String> {
        if self.request.name.trim().is_empty() {
            return Err("the name cannot be empty".to_string());
        }
        self.request.extra_args = crate::commands::install::split_args(&self.args_text);
        self.request.comment = self.request.comment.take().filter(|c| !c.trim().is_empty());

        let icon = self.icon_text.trim();
        self.request.icon = if icon.is_empty() {
            IconChoice::Embedded
        } else {
            let path = PathBuf::from(icon);
            if !path.is_file() {
                return Err(format!("{} is not a file", path.display()));
            }
            IconChoice::File(path)
        };

        let source = self.update_source_text.trim();
        self.request.update_source = if source.is_empty() {
            None
        } else {
            Some(update::parse_update_source(source).map_err(|error| error.to_string())?)
        };
        Ok(())
    }
}

fn index_of(category: &str) -> Option<usize> {
    MAIN_CATEGORIES.iter().position(|c| *c == category)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form_for(info: AppImageInfo) -> InstallForm {
        let path = Path::new("/home/me/Downloads/Fake_App-1.0.0-x86_64.AppImage");
        InstallForm::new(path, &path.to_string_lossy(), info)
    }

    fn linking(repository: &str) -> AppImageInfo {
        AppImageInfo {
            name: Some("Fake App".to_string()),
            suggested_update_source: Some(repository.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn the_appstream_suggestion_is_prefilled_and_installed_once_confirmed() {
        let mut form = form_for(linking("github:fake/app"));
        assert_eq!(form.text(Field::UpdateSource), "github:fake/app");
        assert!(form.suggested);
        // Nothing is decided before the form is finished.
        assert_eq!(form.request.update_source, None);

        form.finish().unwrap();
        assert_eq!(form.request.update_source.as_deref(), Some("github:fake/app"));
    }

    #[test]
    fn embedded_update_information_leaves_the_field_empty() {
        let mut info = linking("github:fake/app");
        info.update_info = Some("zsync|https://example.com/Fake_App.AppImage.zsync".to_string());
        let mut form = form_for(info);
        assert_eq!(form.text(Field::UpdateSource), "");
        assert!(!form.suggested);
        form.finish().unwrap();
        assert_eq!(form.request.update_source, None);
    }

    #[test]
    fn the_field_is_checked_and_stored_in_one_spelling() {
        let mut form = form_for(linking("github:fake/app"));

        form.update_source_text = "  ".to_string();
        form.finish().unwrap();
        assert_eq!(form.request.update_source, None);

        form.update_source_text = "https://github.com/o/r/releases".to_string();
        form.finish().unwrap();
        assert_eq!(form.request.update_source.as_deref(), Some("github:o/r"));

        form.update_source_text = "o/r".to_string();
        let problem = form.finish().unwrap_err();
        assert!(problem.contains("is not an update source"), "{problem}");
    }
}

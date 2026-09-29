use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

use brew_io::{PackageSearchKind, PackageSearchResult, create_api_client_with_optional_cache};

use crate::ui::{PromptDefault, StdUi, Ui};

const PAGE_SIZE: usize = 20;

pub async fn execute(
    root: &Path,
    query_terms: Vec<String>,
    ui: &mut StdUi,
) -> Result<(), brew_core::Error> {
    let query = query_terms.join(" ");
    let client = create_api_client_with_optional_cache(root)?;
    let results = client.search_packages(&query).await?;

    let interactive = std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && results.len() > PAGE_SIZE;
    let mut stdin = io::stdin().lock();
    render_search_results_with_reader(ui, &results, &query, interactive, &mut stdin)
        .map_err(ui_error)
}

fn render_search_results_with_reader<O: Write, E: Write, R: BufRead>(
    ui: &mut Ui<O, E>,
    results: &[PackageSearchResult],
    query: &str,
    interactive: bool,
    reader: &mut R,
) -> io::Result<()> {
    if results.is_empty() {
        return ui.println(format!("No formulas or casks found for '{}'.", query));
    }

    for (index, result) in results.iter().enumerate() {
        ui.println(format_search_result(result))?;

        let has_more = index + 1 < results.len();
        let page_boundary = (index + 1) % PAGE_SIZE == 0;
        if interactive
            && has_more
            && page_boundary
            && !ui.prompt_yes_no_with_reader(
                "Show next 20 results? [Y/n]",
                PromptDefault::Yes,
                reader,
            )?
        {
            break;
        }
    }

    Ok(())
}

fn format_search_result(result: &PackageSearchResult) -> String {
    let kind = match result.kind {
        PackageSearchKind::Formula => "formula",
        PackageSearchKind::Cask => "cask",
    };

    match result.kind {
        PackageSearchKind::Formula => format!("{kind:<8} {}", result.install_name),
        PackageSearchKind::Cask => match &result.display_name {
            Some(display_name) => {
                format!("{kind:<8} {} ({display_name})", result.install_name)
            }
            None => format!("{kind:<8} {}", result.install_name),
        },
    }
}

fn ui_error(err: io::Error) -> brew_core::Error {
    brew_core::Error::StoreCorruption {
        message: format!("failed to write CLI output: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn formula(name: &str) -> PackageSearchResult {
        PackageSearchResult {
            kind: PackageSearchKind::Formula,
            install_name: name.to_string(),
            display_name: None,
        }
    }

    fn cask(token: &str, display_name: Option<&str>) -> PackageSearchResult {
        PackageSearchResult {
            kind: PackageSearchKind::Cask,
            install_name: format!("cask:{token}"),
            display_name: display_name.map(ToString::to_string),
        }
    }

    #[test]
    fn format_search_result_includes_cask_display_name() {
        let rendered =
            format_search_result(&cask("visual-studio-code", Some("Visual Studio Code")));
        assert_eq!(
            rendered,
            "cask     cask:visual-studio-code (Visual Studio Code)"
        );
    }

    #[test]
    fn render_search_results_prints_all_results_when_non_interactive() {
        let results = vec![
            formula("code-server"),
            cask("visual-studio-code", Some("Visual Studio Code")),
        ];
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();
        let mut ui = Ui::with_writers(&mut out, &mut err);
        let mut input = Cursor::new(Vec::<u8>::new());

        render_search_results_with_reader(&mut ui, &results, "code", false, &mut input).unwrap();

        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("formula  code-server"));
        assert!(out.contains("cask     cask:visual-studio-code (Visual Studio Code)"));
        assert!(!out.contains("Show next 20 results?"));
    }

    #[test]
    fn render_search_results_pages_and_continues_on_yes() {
        let results = (0..21)
            .map(|index| formula(&format!("code-{index:02}")))
            .collect::<Vec<_>>();
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();
        let mut ui = Ui::with_writers(&mut out, &mut err);
        let mut input = Cursor::new(b"\n".to_vec());

        render_search_results_with_reader(&mut ui, &results, "code", true, &mut input).unwrap();

        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("formula  code-00"));
        assert!(out.contains("formula  code-20"));
        assert!(out.contains("Show next 20 results? [Y/n]"));
    }

    #[test]
    fn render_search_results_stops_after_declined_next_page() {
        let results = (0..21)
            .map(|index| formula(&format!("code-{index:02}")))
            .collect::<Vec<_>>();
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();
        let mut ui = Ui::with_writers(&mut out, &mut err);
        let mut input = Cursor::new(b"n\n".to_vec());

        render_search_results_with_reader(&mut ui, &results, "code", true, &mut input).unwrap();

        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("formula  code-19"));
        assert!(!out.contains("formula  code-20"));
    }

    #[test]
    fn render_search_results_prints_empty_state() {
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();
        let mut ui = Ui::with_writers(&mut out, &mut err);
        let mut input = Cursor::new(Vec::<u8>::new());

        render_search_results_with_reader(&mut ui, &[], "code", false, &mut input).unwrap();

        let out = String::from_utf8(out).unwrap();
        assert_eq!(out.trim(), "No formulas or casks found for 'code'.");
    }
}

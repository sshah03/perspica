use crate::MultiFileResult;

pub fn render_multi(multi: &MultiFileResult) {
    match serde_json::to_string_pretty(multi) {
        Ok(json) => println!("{json}"),
        Err(e) => eprintln!("JSON serialization error: {e}"),
    }
}

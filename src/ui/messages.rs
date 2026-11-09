#[derive(Clone, Copy, Debug, Default)]
pub struct Messages;

impl Messages {
    pub fn get(self, key: &str) -> &'static str {
        match key {
            "title" => "Herdr Finder",
            "search" => "Search agents and terminals",
            "loading" => "Loading workspaces...",
            "empty" => "No matches",
            "lost" => "The selected terminal disappeared. Choose another row.",
            "indexing" => "Searching current terminal screens...",
            "partial" => "Some terminal screens could not be read. Ctrl+R refreshes.",
            "opening" => "Opening the selected terminal...",
            "group" => "Enter or Space folds this group. Arrows browse groups.",
            "group_search" => "Search keeps matching groups expanded.",
            "conversation" => "Last message",
            "keys" => "Enter browse  Esc browse",
            "list_keys" => "←→ tabs  / search  Enter  Esc",
            "filter" => "Filter",
            "date_unlinked" => "Date unlinked",
            "date_unknown" => "Date unavailable",
            "all" => "All",
            "working" => "Working",
            "blocked" => "Needs input",
            "done" => "Done",
            "idle" => "Idle",
            "unknown" => "Unknown",
            _ => "",
        }
    }
}

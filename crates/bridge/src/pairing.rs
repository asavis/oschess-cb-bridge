//! The pairing link: the oschess page that connects to this bridge, carrying
//! the token and the port in the URL fragment, which browsers never send to a
//! server. The web app reads the fragment and removes it from the address.

/// The oschess site the pairing link opens unless `bridge.toml` names another.
pub const DEFAULT_WEB: &str = "https://oschess.org";

/// The Library's ChessBase section on `web`, with the pairing fragment. The
/// token is base64url, so it needs no escaping.
pub fn link(web: &str, token: &str, port: u16) -> String {
    format!("{}/library?source=chessbase#cb-bridge={token}&port={port}", web.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::TOKEN_LEN;

    #[test]
    fn the_link_opens_the_chessbase_section_with_the_fragment() {
        let token = "aB-_0123456789abcdefghijklmnopqrstuvwxyzABC";
        assert_eq!(token.len(), TOKEN_LEN);
        let link = link(DEFAULT_WEB, token, 39581);
        assert_eq!(link, format!("https://oschess.org/library?source=chessbase#cb-bridge={token}&port=39581"));
        let (page, fragment) = link.split_once('#').unwrap();
        assert!(!page.contains(token), "the token stays in the fragment");
        let fields: Vec<(&str, &str)> = fragment.split('&').map(|f| f.split_once('=').unwrap()).collect();
        assert_eq!(fields, [("cb-bridge", token), ("port", "39581")]);
    }

    #[test]
    fn another_site_and_port() {
        assert_eq!(
            link("https://staging.oschess.org/", "t", 40000),
            "https://staging.oschess.org/library?source=chessbase#cb-bridge=t&port=40000"
        );
    }
}

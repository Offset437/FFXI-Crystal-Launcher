//! Public item lookups with an isolated world preference. FFXIAH gives its
//! browser cookie precedence over a link's `sid`, so an external link alone
//! cannot reliably select a world. These requests never use browser cookies.
use crate::i18n::{t, tf};
use scraper::{Html, Selector};
use std::time::Duration;

const HOST: &str = "www.ffxiah.com";
const MAX_BODY: usize = 2 * 1024 * 1024;
const MAX_RESULTS: usize = 30;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Quote {
    pub name: String,
    pub url: String,
    pub stack: Option<u64>,
    pub price: Option<u64>,
    pub stock: Option<u64>,
    pub last_sale: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Results {
    pub items: Vec<Quote>,
    pub truncated: bool,
    pub warning: Option<String>,
}

#[derive(Default)]
pub(crate) struct Lookup {
    pub open: bool,
    pub query: String,
    pub world: &'static str,
    pub generation: u64,
    pub result: Option<Result<Results, String>>,
}
impl Lookup {
    pub fn begin(&mut self, query: &str, world: &'static str) -> u64 {
        self.open = true;
        self.query = query.trim().to_owned();
        self.world = world;
        self.generation += 1;
        self.result = None;
        self.generation
    }
    pub fn finish(&mut self, generation: u64, result: Result<Results, String>) {
        if self.open && generation == self.generation { self.result = Some(result); }
    }
}

fn request(client: &reqwest::Client, query: &str, sid: u8) -> reqwest::RequestBuilder {
    client.get("https://www.ffxiah.com/search/item")
        .query(&[("q", query.trim())])
        .header(reqwest::header::COOKIE, format!("sid={sid}"))
}

pub(crate) async fn search(query: String, world: &'static str) -> Result<Results, String> {
    let sid = ah_server_id(world).ok_or_else(|| t("Choose a valid FFXIAH world.").to_owned())?;
    if query.trim().is_empty() || query.chars().count() > 160 {
        return Err(t("Enter an item name of up to 160 characters.").into());
    }
    let client = reqwest::Client::builder()
        .user_agent(concat!("Crystal-Launcher/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 8 || attempt.url().scheme() != "https" || attempt.url().host_str() != Some(HOST) {
                attempt.error("Unexpected FFXIAH redirect")
            } else { attempt.follow() }
        }))
        .build().map_err(|e| tf("FFXIAH lookup failed: {error}", &[("error", &e)]))?;
    let (html, url) = read_page(request(&client, &query, sid)).await?;
    // Parsed once; the document is dropped before the next await.
    let (mut results, other_url) = {
        let doc = Html::parse_document(&html);
        (parse(&doc, &url, sid)?, counterpart_url(&doc, &url, sid))
    };
    // Exact searches redirect to one listing; its single/stack sibling has
    // separate stock and sales. Keep the first listing if that fetch fails.
    if let Some(other_url) = other_url {
        let other = async {
            let (html, final_url) = read_page(client.get(other_url.clone())
                .header(reqwest::header::COOKIE, format!("sid={sid}"))).await?;
            if listing_key(&final_url) != listing_key(&other_url) {
                return Err(t("FFXIAH's item page could not be read.").to_owned());
            }
            let mut other = parse(&Html::parse_document(&html), &final_url, sid)?;
            if other.items.len() != 1 || other.items[0].stack.is_some() != listing_key(&other_url).is_some_and(|(_, stack)| stack) {
                return Err(t("FFXIAH's item page could not be read.").to_owned());
            }
            Ok(other.items.remove(0))
        }.await;
        match other {
            Ok(item) => {
                results.items.push(item);
                results.items.sort_by_key(|item| item.stack.is_some());
            }
            Err(error) => results.warning = Some(tf("The other listing could not be loaded: {error}", &[("error", &error)])),
        }
    }
    Ok(results)
}

async fn read_page(request: reqwest::RequestBuilder) -> Result<(String, reqwest::Url), String> {
    let mut response = request.send().await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| tf("FFXIAH lookup failed: {error}", &[("error", &e)]))?;
    let url = response.url().clone();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| tf("FFXIAH lookup failed: {error}", &[("error", &e)]))? {
        if bytes.len().saturating_add(chunk.len()) > MAX_BODY {
            return Err(t("FFXIAH returned too much data. Try a more specific item name.").into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((String::from_utf8_lossy(&bytes).into_owned(), url))
}

fn selector(s: &str) -> Selector { Selector::parse(s).expect("static selector") }
fn plain(element: scraper::ElementRef<'_>) -> String {
    element.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ")
}
fn number(s: &str) -> Option<u64> {
    s.trim().replace(',', "").parse().ok()
}
fn stack_size(label: &str, name: &str) -> Option<u64> {
    label.strip_prefix(name)?.trim().strip_prefix('x').and_then(number).filter(|count| *count > 1)
}
fn listing_key(url: &reqwest::Url) -> Option<(&str, bool)> {
    let id = url.path().strip_prefix("/item/")?.split('/').next()?;
    if id.is_empty() || !id.bytes().all(|c| c.is_ascii_digit()) { return None; }
    Some((id, url.query_pairs().any(|(key, value)| key == "stack" && value == "1")))
}
fn item_summary(doc: &Html) -> Option<scraper::ElementRef<'_>> {
    doc.select(&selector(".item-name")).next()?.ancestors()
        .filter_map(scraper::ElementRef::wrap).find(|element| element.value().name() == "table")
}
fn counterpart_url(doc: &Html, url: &reqwest::Url, sid: u8) -> Option<reqwest::Url> {
    let current = reqwest::Url::parse(&item_url(url.as_str(), url, sid)?).ok()?;
    let (id, stack) = listing_key(&current)?;
    item_summary(doc)?.select(&selector("a[href]")).find_map(|link| {
        let candidate = reqwest::Url::parse(&item_url(link.value().attr("href")?, url, sid)?).ok()?;
        let (other_id, other_stack) = listing_key(&candidate)?;
        (other_id == id && other_stack != stack).then_some(candidate)
    })
}
fn item_url(href: &str, base: &reqwest::Url, sid: u8) -> Option<String> {
    let mut url = base.join(href).ok()?;
    if url.host_str() != Some(HOST) || url.port().is_some() || !url.username().is_empty() || url.password().is_some() { return None; }
    if !matches!(url.scheme(), "http" | "https") { return None; }
    let stack = listing_key(&url)?.1;
    url.set_scheme("https").ok()?;
    url.set_fragment(None);
    url.set_query(None);
    if stack { url.query_pairs_mut().append_pair("stack", "1"); }
    url.query_pairs_mut().append_pair("sid", &sid.to_string());
    Some(url.into())
}

fn parse(doc: &Html, url: &reqwest::Url, sid: u8) -> Result<Results, String> {
    // Never label fallback/default-world data as the requested world's data.
    let selected = doc.select(&selector("#ffxi-main-server-select option[selected]"))
        .next().and_then(|e| e.value().attr("value")).and_then(|v| v.parse::<u8>().ok());
    if selected != Some(sid) { return Err(t("FFXIAH did not confirm the selected world. Retry or use the website.").into()); }
    let table = selector("table.stdlist");
    if let Some(table) = doc.select(&table).next() {
        let row_sel = selector("tr");
        let cell_sel = selector("td");
        let link_sel = selector("a[href]");
        let mut items = Vec::new();
        let mut truncated = false;
        for row in table.select(&row_sel) {
            let cells: Vec<_> = row.select(&cell_sel).collect();
            if cells.len() < 8 { continue; }
            let Some(link) = cells[1].select(&link_sel).next() else { continue; };
            let Some(url) = link.value().attr("href").and_then(|h| item_url(h, url, sid)) else { continue; };
            if items.len() == MAX_RESULTS { truncated = true; break; }
            let name = plain(link);
            if name.is_empty() || name.len() > 512 { continue; }
            let label = plain(cells[1]);
            let stack = stack_size(&label, &name);
            items.push(Quote { name, url, stack, price: number(&plain(cells[5])), stock: number(&plain(cells[6])), last_sale: false });
        }
        return Ok(Results { items, truncated, ..Default::default() });
    }
    if let Some(item_url) = item_url(url.as_str(), url, sid) {
        let name = doc.select(&selector(".item-name .ucwords, #tbl-header h1")).next().map(plain)
            .filter(|n| !n.is_empty() && n.len() <= 512)
            .ok_or_else(|| t("FFXIAH's item page could not be read.").to_owned())?;
        let stack = doc.select(&selector(".item-name")).next().and_then(|element| stack_size(&plain(element), &name));
        let stock = item_summary(doc)
            .and_then(|table| table.select(&selector(".stock")).next())
            .and_then(|element| number(&plain(element)));
        // Exact-name searches redirect to the item page. Read its public sales
        // data as JSON, never execute website scripts or import their code.
        let sales = doc.select(&selector("script")).find_map(|e| {
            let text = e.text().collect::<String>();
            let tail = text.split_once("Item.sales")?.1.trim_start().strip_prefix('=')?.trim_start();
            <Option<Vec<Sale>> as serde::Deserialize>::deserialize(&mut serde_json::Deserializer::from_str(tail)).ok()
        }).ok_or_else(|| t("FFXIAH's item prices could not be read.").to_owned())?;
        return Ok(Results { items: vec![Quote { name, url: item_url, stack,
            price: sales.and_then(|s| s.first().map(|s| s.price)), stock, last_sale: true }], ..Default::default() });
    }
    let title = doc.select(&selector("title")).next().map(plain).unwrap_or_default();
    if title == "Item Search - FFXIAH.com" { return Ok(Results::default()); }
    Err(t("FFXIAH's search results could not be read.").into())
}

#[derive(serde::Deserialize)]
struct Sale { price: u64 }

/// The FFXIAH server picked when the launcher opens.
pub(crate) const DEFAULT_AH_SERVER: &str = "Asura";
/// The active retail worlds with FFXIAH's server ids (its `sid` select).
pub(crate) const AH_SERVERS: [(&str, u8); 16] = [
    ("Asura", 28),
    ("Bahamut", 1),
    ("Bismarck", 25),
    ("Carbuncle", 6),
    ("Cerberus", 23),
    ("Fenrir", 7),
    ("Lakshmi", 27),
    ("Leviathan", 11),
    ("Odin", 12),
    ("Phoenix", 5),
    ("Quetzalcoatl", 16),
    ("Ragnarok", 20),
    ("Shiva", 2),
    ("Siren", 17),
    ("Sylph", 8),
    ("Valefor", 9),
];
/// Open the search for the wiki matching the launcher's selected language.
pub(crate) fn wiki_search_url(query: &str) -> String {
    let url = match crate::i18n::current() {
        crate::i18n::Lang::En => {
            let mut url = reqwest::Url::parse("https://www.bg-wiki.com/index.php").expect("static URL");
            url.query_pairs_mut().append_pair("search", query.trim());
            url
        },
        crate::i18n::Lang::Ja => {
            let mut url = reqwest::Url::parse("https://wiki.ffo.jp/search.cgi").expect("static URL");
            url.query_pairs_mut().extend_pairs([
                ("CCC", "愛"), ("Command", "Search"), ("qf", query.trim()),
                ("order", "match"), ("ffotype", "title"), ("type", "title"),
            ]);
            url
        },
    };
    url.into()
}
pub(crate) fn ah_server_id(server: &str) -> Option<u8> {
    AH_SERVERS.iter().find(|(name, _)| *name == server).map(|(_, id)| *id)
}
/// Browser fallback. FFXIAH's saved world cookie overrides this URL's `sid`;
/// the native lookup sends its own isolated cookie instead.
pub(crate) fn ah_search_url(query: &str, server: &str) -> String {
    let mut url = reqwest::Url::parse("https://www.ffxiah.com/search/item").expect("static URL");
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("q", query.trim());
        if let Some(sid) = ah_server_id(server) {
            pairs.append_pair("sid", &sid.to_string());
        }
    }
    url.into()
}
/// `value` with thousands separators, as FFXIAH shows prices.
pub(crate) fn grouped_number(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) { out.push(','); }
        out.push(c);
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    fn parse(html: &str, url: &reqwest::Url, sid: u8) -> Result<Results, String> {
        super::parse(&Html::parse_document(html), url, sid)
    }
    fn counterpart_url(html: &str, url: &reqwest::Url, sid: u8) -> Option<reqwest::Url> {
        super::counterpart_url(&Html::parse_document(html), url, sid)
    }
    fn page(world: u8, body: &str) -> String {
        format!("<html><select id='ffxi-main-server-select'><option selected value='{world}'>World</option></select>{body}</html>")
    }
    #[test]
    fn lookup_sends_the_world_cookie_that_the_site_uses() {
        let request = request(&reqwest::Client::new(), "  Ore & ingot  ", 25).build().unwrap();
        assert_eq!(request.headers()[reqwest::header::COOKIE], "sid=25");
        assert_eq!(request.url().as_str(), "https://www.ffxiah.com/search/item?q=Ore+%26+ingot");
    }
    #[test]
    fn list_prices_and_stacks_belong_to_the_confirmed_world() {
        let url = reqwest::Url::parse("https://www.ffxiah.com/search/item?q=ore").unwrap();
        let html = page(25, "<table class='stdlist'><tr><td></td><td><a href='http://www.ffxiah.com/item/9001//?stack=1'><span>Test</span> Ore</a> x12</td><td>0</td><td>0</td><td>Description</td><td><span>12,345</span></td><td>7</td><td>2.5</td></tr></table>");
        let results = parse(&html, &url, 25).unwrap();
        assert_eq!((results.items[0].name.as_str(), results.items[0].stack, results.items[0].price, results.items[0].stock), ("Test Ore", Some(12), Some(12345), Some(7)));
        assert_eq!(results.items[0].url, "https://www.ffxiah.com/item/9001//?stack=1&sid=25");
        assert!(parse(&html, &url, 28).is_err());
    }
    #[test]
    fn exact_search_reads_the_last_sale_without_executing_scripts() {
        let url = reqwest::Url::parse("https://www.ffxiah.com/item/9001/test").unwrap();
        let html = page(1, "<span class='item-name'><span class='ucwords'>Test Ore</span></span><script>Item.sales = [{\"price\":4321,\"seller_name\":\"ignored\"}]; throw new Error();</script>");
        let results = parse(&html, &url, 1).unwrap();
        assert_eq!(results.items[0].price, Some(4321));
        assert!(results.items[0].last_sale);
        assert_eq!(parse(&html.replace("[{\"price\":4321,\"seller_name\":\"ignored\"}]", "null"), &url, 1).unwrap().items[0].price, None);
    }
    #[test]
    fn exact_search_reads_current_stock_from_the_item_summary() {
        let url = reqwest::Url::parse("https://www.ffxiah.com/item/9001/test").unwrap();
        for (reported, expected) in [("7", Some(7)), ("0", Some(0)), ("1,234", Some(1234)), ("", None), ("unknown", None)] {
            let html = page(25, &format!(
                "<span class=stock>999</span><table class=stdtbl>\
                <tr><td><span class=item-name><span class=ucwords>Test Ore</span></span></td></tr>\
                <tr><td>Stock</td><td><span class=stock>{reported}</span></td></tr></table>\
                <script>Item.sales = [{{\"price\":4321}}];</script>"));
            let result = parse(&html, &url, 25).unwrap();
            assert_eq!(result.items[0].stock, expected, "reported stock: {reported:?}");
            assert_eq!(result.items[0].price, Some(4321));
            assert!(result.items[0].last_sale);
            assert!(parse(&html, &url, 28).is_err());
        }
    }
    #[test]
    fn exact_search_does_not_substitute_an_unrelated_items_stock() {
        let url = reqwest::Url::parse("https://www.ffxiah.com/item/9001/test").unwrap();
        let html = page(25, "<table class=stdtbl><tr><td><span class=item-name><span class=ucwords>Test Ore</span></span></td></tr></table>\
            <span class=stock>999</span><script>Item.sales = null;</script>");
        let result = parse(&html, &url, 25).unwrap();
        assert_eq!(result.items[0].stock, None);
        assert_eq!(result.items[0].price, None);
    }
    #[test]
    fn exact_stack_listing_has_its_own_quantity_price_and_stock() {
        let url = reqwest::Url::parse("https://www.ffxiah.com/item/9001/test/?stack=1").unwrap();
        let html = page(25, "<table class=stdtbl><tr><td><span class=item-name><span class=ucwords>Test Ore</span>&nbsp;x12</span></td></tr>\
            <tr><td>Stock</td><td><span class=stock>3</span></td></tr></table><script>Item.sales = [{\"price\":24000}];</script>");
        let item = parse(&html, &url, 25).unwrap().items.remove(0);
        assert_eq!((item.stack, item.price, item.stock), (Some(12), Some(24000), Some(3)));
        assert_eq!(item.url, "https://www.ffxiah.com/item/9001/test/?stack=1&sid=25");
        assert!(item.last_sale);
    }
    #[test]
    fn only_the_same_items_other_listing_can_be_requested() {
        let url = reqwest::Url::parse("https://www.ffxiah.com/item/9001/test").unwrap();
        let bad_links = "<a href='https://example.com/item/9001/?stack=1'>foreign</a>\
            <a href='/item/9002/?stack=1'>other item</a><a href='/item/9001/test'>same listing</a>\
            <a href='javascript:alert(1)'>script</a>";
        let html = |links: &str| page(25, &format!("<table class=stdtbl><tr><td><span class=item-name><span class=ucwords>Test Ore</span></span>{links}</td></tr></table>"));
        assert!(counterpart_url(&html(bad_links), &url, 25).is_none());
        let with_stack = html(&format!("{bad_links}<a href='/item/9001/test/?stack=1&sid=28'>stack</a>"));
        assert_eq!(counterpart_url(&with_stack, &url, 25).unwrap().as_str(),
            "https://www.ffxiah.com/item/9001/test/?stack=1&sid=25");
        let stack_url = reqwest::Url::parse("https://www.ffxiah.com/item/9001/test/?stack=1").unwrap();
        assert_eq!(counterpart_url(&html("<a href='/item/9001/test'>single</a>"), &stack_url, 25).unwrap().as_str(),
            "https://www.ffxiah.com/item/9001/test?sid=25");
        let search_url = reqwest::Url::parse("https://www.ffxiah.com/search/item?q=test").unwrap();
        assert!(counterpart_url(&with_stack, &search_url, 25).is_none());
    }
    #[test]
    fn unexpected_links_and_unconfirmed_worlds_are_rejected() {
        let base = reqwest::Url::parse("https://www.ffxiah.com/search/item").unwrap();
        for link in ["https://example.com/item/9001", "javascript:alert(1)", "/login", "https://user@www.ffxiah.com/item/9001"] {
            assert!(item_url(link, &base, 28).is_none());
        }
        assert!(parse("<title>Blocked</title>", &base, 28).is_err());
    }
    #[test]
    fn old_requests_cannot_replace_a_new_world_or_reopen_a_closed_lookup() {
        let mut lookup = Lookup::default();
        let old = lookup.begin("ore", "Asura");
        let current = lookup.begin("ore", "Bismarck");
        lookup.finish(old, Ok(Results::default()));
        assert!(lookup.result.is_none());
        lookup.finish(current, Ok(Results::default()));
        assert!(lookup.result.is_some());
        lookup.open = false;
        lookup.finish(current, Err("old".into()));
        assert!(lookup.result.as_ref().unwrap().is_ok());
    }
    #[tokio::test]
    #[ignore = "Read-only public FFXIAH lookup; requires internet"]
    async fn live_lookup_confirms_two_worlds_and_an_exact_item() {
        for world in ["Asura", "Bismarck"] {
            let list = search("copper".into(), world).await.unwrap();
            assert!(!list.items.is_empty());
            println!("{world}: {} matches, first price {:?}", list.items.len(), list.items[0].price);
            let exact = search("Copper Ore".into(), world).await.unwrap();
            assert_eq!(exact.items.len(), 2);
            assert!(exact.warning.is_none(), "{:?}", exact.warning);
            assert_eq!(exact.items[0].stack, None);
            assert_eq!(exact.items[1].stack, Some(12));
            for item in exact.items {
                assert!(item.stock.is_some());
                println!("{world}: exact lookup {} stack {:?}, price {:?}, stock {:?}", item.name, item.stack, item.price, item.stock);
            }
        }
    }
}

#[cfg(test)]
mod link_tests {
    use super::*;

    #[test]
    fn wiki_search_encodes_the_query() {
        crate::i18n::set(crate::i18n::Lang::En);
        assert_eq!(
            wiki_search_url("  Sortie gear & aug "),
            "https://www.bg-wiki.com/index.php?search=Sortie+gear+%26+aug"
        );
        crate::i18n::set(crate::i18n::Lang::Ja);
        let search = wiki_search_url("  サルベージ & 装備  ");
        assert!(crate::allowed_link(&search));
        let url = reqwest::Url::parse(&search).unwrap();
        assert_eq!(url.host_str(), Some("wiki.ffo.jp"));
        assert_eq!(url.path(), "/search.cgi");
        let pairs: std::collections::BTreeMap<_, _> = url.query_pairs().collect();
        assert_eq!(pairs.len(), 6);
        assert_eq!(pairs.get("qf").unwrap(), "サルベージ & 装備");
        assert_eq!(pairs.get("CCC").unwrap(), "愛");
        assert_eq!(pairs.get("Command").unwrap(), "Search");
        crate::i18n::set(crate::i18n::Lang::En);
        assert_eq!(wiki_search_url("Sortie"), "https://www.bg-wiki.com/index.php?search=Sortie");
    }

    #[test]
    fn ah_browser_fallback_encodes_the_query_and_world_hint() {
        assert_eq!(ah_search_url("Ridill", "Asura"), "https://www.ffxiah.com/search/item?q=Ridill&sid=28");
        assert_eq!(ah_search_url("Hope Staff", "Bismarck"), "https://www.ffxiah.com/search/item?q=Hope+Staff&sid=25");
        assert_eq!(ah_search_url("Ridill", "Nowhere"), "https://www.ffxiah.com/search/item?q=Ridill");
    }

    #[test]
    fn every_listed_server_has_a_distinct_id() {
        let mut ids: Vec<u8> = AH_SERVERS.iter().map(|(_, id)| *id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), AH_SERVERS.len());
        assert_eq!(AH_SERVERS[0].0, DEFAULT_AH_SERVER);
        assert!(crate::allowed_link(&wiki_search_url("x")));
        assert!(crate::allowed_link(&ah_search_url("x", "Asura")));
    }

    #[test]
    fn prices_group_thousands() {
        assert_eq!(grouped_number(0), "0");
        assert_eq!(grouped_number(999), "999");
        assert_eq!(grouped_number(1_000), "1,000");
        assert_eq!(grouped_number(12_345_678), "12,345,678");
    }
}

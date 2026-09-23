//! .nuspec → JSON reproducing fast-xml-parser 5 with { ignoreAttributes: false, parseTagValue: false }
//! for the <package><metadata> subtree, plus top-level name/version.

use crate::error::{Error, Result};
use quick_xml::Reader;
use quick_xml::events::Event;
use serde_json::{Map, Value};

struct Node {
    name: String,
    attributes: Vec<(String, String)>,
    children: Vec<Child>,
}

enum Child {
    Element(Node),
    Text(String),
}

fn err(message: impl Into<String>) -> Error {
    Error::validation(format!("invalid nuspec: {}", message.into()))
}

/// ECMAScript String.prototype.trim() whitespace set.
fn js_trim(value: &str) -> &str {
    value.trim_matches(|c: char| {
        matches!(
            c,
            '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
                ..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
        )
    })
}

/// fast-xml-parser decodes only the five predefined entities, sequentially, ampersand last.
fn decode(value: &str) -> String {
    let mut out = value.to_string();
    for (entity, replacement) in [("&apos;", "'"), ("&gt;", ">"), ("&lt;", "<"), ("&quot;", "\""), ("&amp;", "&")] {
        if out.contains(entity) {
            out = out.replace(entity, replacement);
        }
    }
    out
}

fn sanitize_name(name: &str) -> Result<String> {
    match name {
        "__proto__" | "constructor" | "prototype" => Err(err(format!("element name <{name}> is not allowed"))),
        "hasOwnProperty" | "toString" | "valueOf" | "__defineGetter__" | "__defineSetter__" | "__lookupGetter__" | "__lookupSetter__" => {
            Ok(format!("__{name}"))
        }
        _ => Ok(name.to_string()),
    }
}

fn read_tree(text: &str) -> Result<Node> {
    let mut reader = Reader::from_str(text);
    let config = reader.config_mut();
    config.expand_empty_elements = false;
    config.trim_text(false);
    config.check_end_names = true;
    config.allow_dangling_amp = true;
    let mut stack: Vec<Node> = vec![Node { name: String::new(), attributes: Vec::new(), children: Vec::new() }];
    let mut run = String::new();
    fn flush(run: &mut String, node: &mut Node) {
        if run.is_empty() {
            return;
        }
        let trimmed = js_trim(run);
        if !trimmed.is_empty() {
            node.children.push(Child::Text(decode(trimmed)));
        }
        run.clear();
    }
    loop {
        let event = reader.read_event().map_err(|e| err(e.to_string()))?;
        let event_kind = if matches!(event, Event::Empty(_)) { Kind::Empty } else { Kind::Other };
        match event {
            Event::Eof => break,
            Event::Decl(_) | Event::Comment(_) | Event::DocType(_) => {}
            Event::PI(_) => {
                if stack.len() > 1 {
                    return Err(err("processing instructions are not supported"));
                }
            }
            Event::Text(text) => run.push_str(&text.into_inner()),
            Event::GeneralRef(reference) => {
                run.push('&');
                run.push_str(&reference);
                run.push(';');
            }
            Event::CData(cdata) => {
                let parent = stack.last_mut().unwrap();
                flush(&mut run, parent);
                let content = cdata.into_inner();
                if !content.is_empty() {
                    parent.children.push(Child::Text(content.into_owned()));
                }
            }
            Event::Start(start) | Event::Empty(start) => {
                let is_empty = matches!(event_kind, Kind::Empty);
                let parent = stack.last_mut().unwrap();
                flush(&mut run, parent);
                let name = start.name().as_ref().to_string();
                let mut attributes = Vec::new();
                for attribute in start.attributes().with_checks(true) {
                    let attribute = attribute.map_err(|e| err(format!("attribute error: {e}")))?;
                    let key = attribute.key.as_ref().to_string();
                    attributes.push((key, decode(js_trim(&attribute.value)).to_string()));
                }
                stack.push(Node { name, attributes, children: Vec::new() });
                if is_empty {
                    close(&mut stack)?;
                }
            }
            Event::End(_) => {
                let node = stack.last_mut().unwrap();
                flush(&mut run, node);
                close(&mut stack)?;
            }
        }
    }
    if stack.len() != 1 {
        return Err(err("unclosed element"));
    }
    let mut root = stack.pop().unwrap();
    flush(&mut run, &mut root);
    Ok(root)
}

enum Kind {
    Empty,
    Other,
}

fn close(stack: &mut Vec<Node>) -> Result<()> {
    if stack.len() < 2 {
        return Err(err("unexpected closing tag"));
    }
    let node = stack.pop().unwrap();
    stack.last_mut().unwrap().children.push(Child::Element(node));
    Ok(())
}

fn convert(node: &Node) -> Result<Value> {
    let mut object = Map::new();
    let mut text = String::new();
    for child in &node.children {
        match child {
            Child::Text(piece) => text.push_str(piece),
            Child::Element(element) => {
                let name = sanitize_name(&element.name)?;
                let value = convert(element)?;
                match object.get_mut(&name) {
                    None => {
                        object.insert(name, value);
                    }
                    Some(Value::Array(items)) => items.push(value),
                    Some(existing) => {
                        let first = existing.take();
                        *existing = Value::Array(vec![first, value]);
                    }
                }
            }
        }
    }
    if !text.is_empty() {
        object.insert("#text".to_string(), Value::String(text));
    }
    for (key, value) in &node.attributes {
        object.insert(format!("@_{key}"), Value::String(value.clone()));
    }
    if !node.attributes.is_empty() {
        return Ok(Value::Object(object));
    }
    if object.len() == 1 && object.contains_key("#text") {
        return Ok(object.remove("#text").unwrap());
    }
    if object.is_empty() {
        return Ok(Value::String(String::new()));
    }
    Ok(Value::Object(object))
}

pub fn parse(text: &str) -> Result<Value> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("<!doctype") || lower.contains("<!entity") {
        return Err(err("DTD is not supported"));
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let normalised = text.replace("\r\n", "\n").replace('\r', "\n");
    let root = read_tree(&normalised)?;
    let mut packages = root.children.iter().filter_map(|c| match c {
        Child::Element(e) if e.name == "package" => Some(e),
        _ => None,
    });
    let Some(package) = packages.next() else { return Err(err("missing <package> root element")) };
    if packages.next().is_some() || root.children.iter().any(|c| matches!(c, Child::Element(e) if e.name != "package") || matches!(c, Child::Text(_))) {
        return Err(err("expected exactly one <package> root element"));
    }
    let mut candidates = package.children.iter().filter_map(|c| match c {
        Child::Element(e) if e.name == "metadata" => Some(e),
        _ => None,
    });
    let Some(metadata) = candidates.next() else { return Err(err("missing <metadata> element")) };
    if candidates.next().is_some() {
        return Err(err("expected exactly one <metadata> element"));
    }
    let value = convert(metadata)?;
    let Value::Object(mut object) = value else { return Err(err("<metadata> has no child elements")) };
    let id = object.get("id").cloned().unwrap_or(Value::Null);
    let version = object.get("version").cloned().unwrap_or(Value::Null);
    object.insert("name".to_string(), id);
    object.insert("version".to_string(), version);
    Ok(Value::Object(object))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::canonical_string;

    fn meta(inner: &str) -> String {
        canonical_string(&parse(&format!("<package><metadata><id>Demo</id><version>1.0</version>{inner}</metadata></package>")).unwrap())
    }

    #[test]
    fn matches_fast_xml_parser() {
        assert_eq!(
            meta("<license type=\"expression\">MIT</license>"),
            "{\"id\":\"Demo\",\"license\":{\"#text\":\"MIT\",\"@_type\":\"expression\"},\"name\":\"Demo\",\"version\":\"1.0\"}"
        );
        assert!(meta("<dependencies><group targetFramework=\"net8.0\"><dependency id=\"A\" version=\"1.0\"/><dependency id=\"B\" version=\"2.0\"/></group><group targetFramework=\"net9.0\"/></dependencies>").contains("\"group\":[{\"@_targetFramework\":\"net8.0\",\"dependency\":[{\"@_id\":\"A\",\"@_version\":\"1.0\"},{\"@_id\":\"B\",\"@_version\":\"2.0\"}]},{\"@_targetFramework\":\"net9.0\"}]"));
        assert!(meta("<description><![CDATA[Hello <b>world</b>]]></description>").contains("\"description\":\"Hello <b>world</b>\""));
        assert!(meta("<k><![CDATA[ x ]]><![CDATA[ y ]]></k><j>  <![CDATA[]]>  </j>").contains("\"j\":\"\",\"k\":\" x  y \""));
        assert!(meta("<q>a<r/>b</q>").contains("\"q\":{\"#text\":\"ab\",\"r\":\"\"}"));
        assert!(meta("<a>1</a><a>2</a><a><b/></a>").contains("\"a\":[\"1\",\"2\",{\"b\":\"\"}]"));
        assert!(meta("<x>a <!-- c --> b</x>").contains("\"x\":\"a  b\""));
        assert!(
            meta("<e>&amp;lt; &quot;q&quot; &apos;s&apos; &gt; &foo; &#65; &#x42; & bare</e>")
                .contains("\"e\":\"&lt; \\\"q\\\" 's' > &foo; &#65; &#x42; & bare\"")
        );
        assert!(meta("<m t=\"1\"></m><n t=\"1\">  </n>").contains("\"m\":{\"@_t\":\"1\"},\"n\":{\"@_t\":\"1\"}"));
        assert!(meta("<y> text <z>i</z> tail </y>").contains("\"y\":{\"#text\":\"texttail\",\"z\":\"i\"}"));
        assert!(meta("<toString>x</toString>").contains("\"__toString\":\"x\""));
        assert!(meta("<ns:tag ns:attr=\"v\">t</ns:tag>").contains("\"ns:tag\":{\"#text\":\"t\",\"@_ns:attr\":\"v\"}"));
        assert!(
            meta("<u>\u{a0} nb \u{a0}</u><w>a\r\nb</w><empty></empty><s>  </s>")
                .contains("\"empty\":\"\",\"id\":\"Demo\",\"name\":\"Demo\",\"s\":\"\",\"u\":\"nb\",\"version\":\"1.0\",\"w\":\"a\\nb\"")
        );
        assert!(
            meta("<t>&#xD;&#10;x</t><c>a&#38;b</c><g>x&amp;amp;y</g><h attr=\"&lt;&amp;&#65;\">v</h>")
                .contains("\"c\":\"a&#38;b\",\"g\":\"x&amp;y\",\"h\":{\"#text\":\"v\",\"@_attr\":\"<&&#65;\"}")
        );
    }

    #[test]
    fn root_and_hoisting() {
        let value = parse("<?xml version=\"1.0\"?><package xmlns=\"http://x\"><metadata minClientVersion=\"2.12\"><id>Demo</id><version>1.0</version></metadata><files><file src=\"a\"/></files></package>").unwrap();
        assert_eq!(canonical_string(&value), "{\"@_minClientVersion\":\"2.12\",\"id\":\"Demo\",\"name\":\"Demo\",\"version\":\"1.0\"}");
        assert!(parse("<!DOCTYPE x><package><metadata><id>a</id><version>1</version></metadata></package>").is_err());
        assert!(parse("<package><metadata><id>a</id><version>1</version></metadata><metadata/></package>").is_err());
        assert!(parse("<package><metadata><id>a</id><version>1</version><p><?pi?>t</p></metadata></package>").is_err());
        assert!(parse("<package><metadata><id>a</id><version>1</version><__proto__>x</__proto__></metadata></package>").is_err());
        assert!(parse("<package><metadata><id>a</id><version>1</version><d a=\"1\" a=\"2\"/></metadata></package>").is_err());
    }
}

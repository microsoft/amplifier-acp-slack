// Small PDF documents built in memory, shared by the unit and integration tests.
use lopdf::{
    Document, Object, Stream,
    content::{Content, Operation},
    dictionary,
};

/// One entry per page; each line of a page is shown on its own line. An
/// empty page has no text, as a scanned page has none.
pub fn document(pages: &[&str]) -> Vec<u8> {
    build(pages, None)
}

/// A one-page document whose content inflates to `inflated` bytes.
pub fn inflating(inflated: usize) -> Vec<u8> {
    build(&[""], Some(inflated))
}

fn build(pages: &[&str], inflated: Option<usize>) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font_id },
    });
    let mut kids = Vec::new();
    for page in pages {
        let mut operations = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 11.into()]),
            Operation::new("TL", vec![14.into()]),
            Operation::new("Td", vec![72.into(), 720.into()]),
        ];
        for line in page.lines() {
            operations.push(Operation::new("'", vec![Object::string_literal(line)]));
        }
        operations.push(Operation::new("ET", vec![]));
        let mut encoded = Content { operations }.encode().unwrap();
        if let Some(inflated) = inflated {
            encoded = vec![b' '; inflated];
        }
        let mut stream = Stream::new(dictionary! {}, encoded);
        if inflated.is_some() {
            stream.compress().unwrap();
        }
        let content_id = doc.add_object(stream);
        kids.push(Object::from(doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
        })));
    }
    let count = kids.len() as i64;
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => kids, "Count" => count,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        }),
    );
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog_id);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

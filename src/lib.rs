#![deny(clippy::all)]

#[macro_use]
extern crate napi_derive;

use std::collections::BTreeMap;
use lopdf::{Document, Object, ObjectId};
use napi::{CallContext, Env, JsNumber, JsObject, Result, Task, JsBuffer};

#[cfg(all(
unix,
not(target_env = "musl"),
not(target_arch = "aarch64"),
not(target_arch = "arm"),
not(debug_assertions)
))]
#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

#[cfg(all(windows, target_arch = "x86_64"))]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

struct AsyncTask(u32);

impl Task for AsyncTask {
  type Output = u32;
  type JsValue = JsNumber;

  fn compute(&mut self) -> Result<Self::Output> {
    use std::thread::sleep;
    use std::time::Duration;
    sleep(Duration::from_millis(self.0 as u64));
    Ok(self.0 * 2)
  }

  fn resolve(self, env: Env, output: Self::Output) -> Result<Self::JsValue> {
    env.create_uint32(output)
  }
}

#[module_exports]
fn init(mut exports: JsObject) -> Result<()> {
  exports.create_named_method("mergePdf", merge_documents)?;
  Ok(())
}

#[js_function(1)]
fn merge_documents(ctx: CallContext) -> Result<JsBuffer> {
  // Should read `Array<Buffer>`
  let buffers = ctx.get::<JsObject>(0)?;
  let len_arr = vec![0; buffers.get_array_length()? as usize]; // Create the array iter
  let doc_buffers = len_arr.iter()
      .enumerate() // Add the index to the element
      .map(|(i, _)| {
        let buffer = &mut buffers
            .get_named_property::<JsBuffer>(i.to_string().as_str())
            .unwrap()
            .into_value()
            .unwrap()
            .to_vec();
        // Load the pdf by memory
        Document::load_mem(&buffer).unwrap()
      })
      .collect::<Vec<Document>>();
  // Add buffer to target
  let mut target: Vec<u8> = vec![];
  merge_documents_to(&doc_buffers, &mut target);
  Ok(ctx.env.create_buffer_with_data(target).unwrap().into_raw())
}

#[inline]
fn merge_documents_to(documents: &Vec<Document>, target: &mut Vec<u8>) {
  let documents = documents.clone();
  // Define a starting max_id (will be used as start index for object_ids)
  let mut max_id = 1;
  // Collect all Documents Objects grouped by a map
  let mut documents_pages = BTreeMap::new();
  let mut documents_objects = BTreeMap::new();
  for mut document in documents {
    document.renumber_objects_with(max_id);
    max_id = document.max_id + 1;
    documents_pages.extend(
      document
          .get_pages()
          .into_iter()
          .map(|(_, object_id)| {
            (
              object_id,
              document.get_object(object_id).unwrap().to_owned(),
            )
          })
          .collect::<BTreeMap<ObjectId, Object>>(),
    );
    documents_objects.extend(document.objects);
  }
  // Initialize a new empty document
  let mut document = Document::with_version("1.5");
  // Catalog and Pages are mandatory
  let mut catalog_object: Option<(ObjectId, Object)> = None;
  let mut pages_object: Option<(ObjectId, Object)> = None;
  // Process all objects except "Page" type
  for (object_id, object) in documents_objects.iter() {
    // We have to ignore "Page" (as are processed later), "Outlines" and "Outline" objects
    // All other objects should be collected and inserted into the main Document
    match object.type_name().unwrap_or("") {
      "Catalog" => {
        // Collect a first "Catalog" object and use it for the future "Pages"
        catalog_object = Some((
          if let Some((id, _)) = catalog_object {
            id
          } else {
            *object_id
          },
          object.clone(),
        ));
      }
      "Pages" => {
        // Collect and update a first "Pages" object and use it for the future "Catalog"
        // We have also to merge all dictionaries of the old and the new "Pages" object
        if let Ok(dictionary) = object.as_dict() {
          let mut dictionary = dictionary.clone();
          if let Some((_, ref object)) = pages_object {
            if let Ok(old_dictionary) = object.as_dict() {
              dictionary.extend(old_dictionary);
            }
          }
          pages_object = Some((
            if let Some((id, _)) = pages_object {
              id
            } else {
              *object_id
            },
            Object::Dictionary(dictionary),
          ));
        }
      }
      "Page" => {}     // Ignored, processed later and separately
      "Outlines" => {} // Ignored, not supported yet
      "Outline" => {}  // Ignored, not supported yet
      _ => {
        document.objects.insert(*object_id, object.clone());
      }
    }
  }
  // If no "Pages" found abort
  if pages_object.is_none() {
    println!("Pages root not found.");
    return;
  }
  // Iter over all "Page" and collect with the parent "Pages" created before
  for (object_id, object) in documents_pages.iter() {
    if let Ok(dictionary) = object.as_dict() {
      let mut dictionary = dictionary.clone();
      dictionary.set("Parent", pages_object.as_ref().unwrap().0);
      document
          .objects
          .insert(*object_id, Object::Dictionary(dictionary));
    }
  }
  // If no "Catalog" found abort
  if catalog_object.is_none() {
    println!("Catalog root not found.");
    return;
  }
  let catalog_object = catalog_object.unwrap();
  let pages_object = pages_object.unwrap();
  // Build a new "Pages" with updated fields
  if let Ok(dictionary) = pages_object.1.as_dict() {
    let mut dictionary = dictionary.clone();
    // Set new pages count
    dictionary.set("Count", documents_pages.len() as u32);
    // Set new "Kids" list (collected from documents pages) for "Pages"
    dictionary.set(
      "Kids",
      documents_pages
          .into_iter()
          .map(|(object_id, _)| Object::Reference(object_id))
          .collect::<Vec<_>>(),
    );
    document
        .objects
        .insert(pages_object.0, Object::Dictionary(dictionary));
  }
  // Build a new "Catalog" with updated fields
  if let Ok(dictionary) = catalog_object.1.as_dict() {
    let mut dictionary = dictionary.clone();
    dictionary.set("Pages", pages_object.0);
    dictionary.remove(b"Outlines"); // Outlines not supported in merged PDFs
    document
        .objects
        .insert(catalog_object.0, Object::Dictionary(dictionary));
  }
  document.trailer.set("Root", catalog_object.0);
  // Update the max internal ID as wasn't updated before due to direct objects insertion
  document.max_id = document.objects.len() as u32;
  // Reorder all new Document objects
  document.renumber_objects();
  document.compress();
  // Save the merged PDF
  document.save_to(target).unwrap();
}

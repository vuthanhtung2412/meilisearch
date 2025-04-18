use std::cell::RefCell;

use bumpalo::Bump;
use hashbrown::HashMap;

use super::DelAddRoaringBitmap;
use crate::constants::RESERVED_GEO_FIELD_NAME;
use crate::update::new::channel::DocumentsSender;
use crate::update::new::document::{write_to_obkv, Document as _};
use crate::update::new::indexer::document_changes::{DocumentChangeContext, Extractor};
use crate::update::new::ref_cell_ext::RefCellExt as _;
use crate::update::new::thread_local::FullySend;
use crate::update::new::DocumentChange;
use crate::vector::EmbeddingConfigs;
use crate::Result;

pub struct DocumentsExtractor<'a, 'b> {
    document_sender: DocumentsSender<'a, 'b>,
    embedders: &'a EmbeddingConfigs,
}

impl<'a, 'b> DocumentsExtractor<'a, 'b> {
    pub fn new(document_sender: DocumentsSender<'a, 'b>, embedders: &'a EmbeddingConfigs) -> Self {
        Self { document_sender, embedders }
    }
}

#[derive(Default)]
pub struct DocumentExtractorData {
    pub docids_delta: DelAddRoaringBitmap,
    pub field_distribution_delta: HashMap<String, i64>,
}

impl<'extractor> Extractor<'extractor> for DocumentsExtractor<'_, '_> {
    type Data = FullySend<RefCell<DocumentExtractorData>>;

    fn init_data(&self, _extractor_alloc: &'extractor Bump) -> Result<Self::Data> {
        Ok(FullySend(Default::default()))
    }

    fn process<'doc>(
        &self,
        changes: impl Iterator<Item = Result<DocumentChange<'doc>>>,
        context: &DocumentChangeContext<Self::Data>,
    ) -> Result<()> {
        let mut document_buffer = bumpalo::collections::Vec::new_in(&context.doc_alloc);
        let mut document_extractor_data = context.data.0.borrow_mut_or_yield();

        for change in changes {
            let change = change?;
            // **WARNING**: the exclusive borrow on `new_fields_ids_map` needs to be taken **inside** of the `for change in changes` loop
            // Otherwise, `BorrowMutError` will occur for document changes that also need the new_fields_ids_map (e.g.: UpdateByFunction)
            let mut new_fields_ids_map = context.new_fields_ids_map.borrow_mut_or_yield();

            let external_docid = change.external_docid().to_owned();

            // document but we need to create a function that collects and compresses documents.
            match change {
                DocumentChange::Deletion(deletion) => {
                    let docid = deletion.docid();
                    let content = deletion.current(
                        &context.rtxn,
                        context.index,
                        &context.db_fields_ids_map,
                    )?;
                    let geo_iter = content
                        .geo_field()
                        .transpose()
                        .map(|res| res.map(|rv| (RESERVED_GEO_FIELD_NAME, rv)));
                    for res in content.iter_top_level_fields().chain(geo_iter) {
                        let (f, _) = res?;
                        let entry = document_extractor_data
                            .field_distribution_delta
                            .entry_ref(f)
                            .or_default();
                        *entry -= 1;
                    }
                    document_extractor_data.docids_delta.insert_del_u32(docid);
                    self.document_sender.delete(docid, external_docid).unwrap();
                }
                DocumentChange::Update(update) => {
                    let docid = update.docid();
                    let content =
                        update.current(&context.rtxn, context.index, &context.db_fields_ids_map)?;
                    let geo_iter = content
                        .geo_field()
                        .transpose()
                        .map(|res| res.map(|rv| (RESERVED_GEO_FIELD_NAME, rv)));
                    for res in content.iter_top_level_fields().chain(geo_iter) {
                        let (f, _) = res?;
                        let entry = document_extractor_data
                            .field_distribution_delta
                            .entry_ref(f)
                            .or_default();
                        *entry -= 1;
                    }
                    let content =
                        update.merged(&context.rtxn, context.index, &context.db_fields_ids_map)?;
                    let geo_iter = content
                        .geo_field()
                        .transpose()
                        .map(|res| res.map(|rv| (RESERVED_GEO_FIELD_NAME, rv)));
                    for res in content.iter_top_level_fields().chain(geo_iter) {
                        let (f, _) = res?;
                        let entry = document_extractor_data
                            .field_distribution_delta
                            .entry_ref(f)
                            .or_default();
                        *entry += 1;
                    }

                    let content =
                        update.merged(&context.rtxn, context.index, &context.db_fields_ids_map)?;
                    let vector_content = update.merged_vectors(
                        &context.rtxn,
                        context.index,
                        &context.db_fields_ids_map,
                        &context.doc_alloc,
                        self.embedders,
                    )?;
                    let content = write_to_obkv(
                        &content,
                        vector_content.as_ref(),
                        &mut new_fields_ids_map,
                        &mut document_buffer,
                    )?;
                    self.document_sender.uncompressed(docid, external_docid, content).unwrap();
                }
                DocumentChange::Insertion(insertion) => {
                    let docid = insertion.docid();
                    let content = insertion.inserted();
                    let geo_iter = content
                        .geo_field()
                        .transpose()
                        .map(|res| res.map(|rv| (RESERVED_GEO_FIELD_NAME, rv)));
                    for res in content.iter_top_level_fields().chain(geo_iter) {
                        let (f, _) = res?;
                        let entry = document_extractor_data
                            .field_distribution_delta
                            .entry_ref(f)
                            .or_default();
                        *entry += 1;
                    }
                    let inserted_vectors =
                        insertion.inserted_vectors(&context.doc_alloc, self.embedders)?;
                    let content = write_to_obkv(
                        &content,
                        inserted_vectors.as_ref(),
                        &mut new_fields_ids_map,
                        &mut document_buffer,
                    )?;
                    document_extractor_data.docids_delta.insert_add_u32(docid);
                    self.document_sender.uncompressed(docid, external_docid, content).unwrap();
                }
            }
        }

        Ok(())
    }
}

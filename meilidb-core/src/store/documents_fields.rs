use std::convert::TryFrom;
use meilidb_schema::SchemaAttr;
use crate::DocumentId;

#[derive(Copy, Clone)]
pub struct DocumentsFields {
    pub(crate) documents_fields: rkv::SingleStore,
}

fn document_attribute_into_key(document_id: DocumentId, attribute: SchemaAttr) -> [u8; 10] {
    let document_id_bytes = document_id.0.to_be_bytes();
    let attr_bytes = attribute.0.to_be_bytes();

    let mut key = [0u8; 10];
    key[0..8].copy_from_slice(&document_id_bytes);
    key[8..10].copy_from_slice(&attr_bytes);

    key
}

fn document_attribute_from_key(key: [u8; 10]) -> (DocumentId, SchemaAttr) {
    let document_id = {
        let array = TryFrom::try_from(&key[0..8]).unwrap();
        DocumentId(u64::from_be_bytes(array))
    };

    let schema_attr = {
        let array = TryFrom::try_from(&key[8..8+2]).unwrap();
        SchemaAttr(u16::from_be_bytes(array))
    };

    (document_id, schema_attr)
}

impl DocumentsFields {
    pub fn put_document_field(
        &self,
        writer: &mut rkv::Writer,
        document_id: DocumentId,
        attribute: SchemaAttr,
        value: &[u8],
    ) -> Result<(), rkv::StoreError>
    {
        let key = document_attribute_into_key(document_id, attribute);
        self.documents_fields.put(writer, key, &rkv::Value::Blob(value))
    }

    pub fn del_all_document_fields(
        &self,
        writer: &mut rkv::Writer,
        document_id: DocumentId,
    ) -> Result<usize, rkv::StoreError>
    {
        let document_id_bytes = document_id.0.to_be_bytes();
        let mut keys_to_delete = Vec::new();

        // WARN we can not delete the keys using the iterator
        //      so we store them and delete them just after
        let iter = self.documents_fields.iter_from(writer, document_id_bytes)?;
        for result in iter {
            let (key, _) = result?;
            let array = TryFrom::try_from(key).unwrap();
            let (current_document_id, _) = document_attribute_from_key(array);
            if current_document_id != document_id { break }

            keys_to_delete.push(key.to_owned());
        }

        let count = keys_to_delete.len();
        for key in keys_to_delete {
            self.documents_fields.delete(writer, key)?;
        }

        Ok(count)
    }

    pub fn document_attribute<'a>(
        &self,
        reader: &'a impl rkv::Readable,
        document_id: DocumentId,
        attribute: SchemaAttr,
    ) -> Result<Option<&'a [u8]>, rkv::StoreError>
    {
        let key = document_attribute_into_key(document_id, attribute);

        match self.documents_fields.get(reader, key)? {
            Some(rkv::Value::Blob(bytes)) => Ok(Some(bytes)),
            Some(value) => panic!("invalid type {:?}", value),
            None => Ok(None),
        }
    }

    pub fn document_fields<'r, T: rkv::Readable>(
        &self,
        reader: &'r T,
        document_id: DocumentId,
    ) -> Result<DocumentFieldsIter<'r>, rkv::StoreError>
    {
        let document_id_bytes = document_id.0.to_be_bytes();
        let iter = self.documents_fields.iter_from(reader, document_id_bytes)?;
        Ok(DocumentFieldsIter { document_id, iter })
    }

    pub fn documents_ids<'r, T: rkv::Readable>(
        &self,
        reader: &'r T,
    ) -> Result<DocumentsIdsIter<'r>, rkv::StoreError>
    {
        let iter = self.documents_fields.iter_start(reader)?;
        Ok(DocumentsIdsIter { last_seen_id: None, iter })
    }
}

pub struct DocumentFieldsIter<'r> {
    document_id: DocumentId,
    iter: rkv::store::single::Iter<'r>,
}

impl<'r> Iterator for DocumentFieldsIter<'r> {
    type Item = Result<(SchemaAttr, &'r [u8]), rkv::StoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.iter.next() {
            Some(Ok((key, Some(rkv::Value::Blob(bytes))))) => {
                let array = TryFrom::try_from(key).unwrap();
                let (current_document_id, attr) = document_attribute_from_key(array);
                if current_document_id != self.document_id { return None; }

                Some(Ok((attr, bytes)))
            },
            Some(Ok((key, data))) => panic!("{:?}, {:?}", key, data),
            Some(Err(e)) => Some(Err(e)),
            None => None,
        }
    }
}

pub struct DocumentsIdsIter<'r> {
    last_seen_id: Option<DocumentId>,
    iter: rkv::store::single::Iter<'r>,
}

impl<'r> Iterator for DocumentsIdsIter<'r> {
    type Item = Result<DocumentId, rkv::StoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        for result in self.iter.next() {
            match result {
                Ok((key, Some(rkv::Value::Blob(bytes)))) => {
                    let array = TryFrom::try_from(key).unwrap();
                    let (document_id, attr) = document_attribute_from_key(array);
                    if Some(document_id) != self.last_seen_id {
                        self.last_seen_id = Some(document_id);
                        return Some(Ok(document_id))
                    }
                },
                Ok((key, data)) => panic!("{:?}, {:?}", key, data),
                Err(e) => return Some(Err(e)),
            }
        }

        None
    }
}

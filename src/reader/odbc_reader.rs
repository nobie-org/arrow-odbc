use std::sync::Arc;

use arrow::{
    datatypes::SchemaRef,
    error::ArrowError,
    record_batch::{RecordBatch, RecordBatchReader},
};
use odbc_api::{buffers::ColumnarAnyBuffer, BlockCursor, Cursor};

use crate::{arrow_schema_from, BufferAllocationOptions, Error, ConcurrentOdbcReader};

use super::{odbc_batch_stream::OdbcBatchStream, to_record_batch::ToRecordBatch};

/// Arrow ODBC reader. Implements the [`arrow::record_batch::RecordBatchReader`] trait so it can be
/// used to fill Arrow arrays from an ODBC data source.
///
/// This reader is generic over the cursor type so it can be used in cases there the cursor only
/// borrows a statement handle (most likely the case then using prepared queries), or owned
/// statement handles (recommened then using one shot queries, to have an easier life with the
/// borrow checker).
///
/// # Example
///
/// ```no_run
/// use arrow_odbc::{odbc_api::{Environment, ConnectionOptions}, OdbcReader};
///
/// const CONNECTION_STRING: &str = "\
///     Driver={ODBC Driver 17 for SQL Server};\
///     Server=localhost;\
///     UID=SA;\
///     PWD=My@Test@Password1;\
/// ";
///
/// fn main() -> Result<(), anyhow::Error> {
///
///     let odbc_environment = Environment::new()?;
///     
///     // Connect with database.
///     let connection = odbc_environment.connect_with_connection_string(
///         CONNECTION_STRING,
///         ConnectionOptions::default()
///     )?;
///
///     // This SQL statement does not require any arguments.
///     let parameters = ();
///
///     // Execute query and create result set
///     let cursor = connection
///         .execute("SELECT * FROM MyTable", parameters)?
///         .expect("SELECT statement must produce a cursor");
///
///     // Each batch shall only consist of maximum 10.000 rows.
///     let max_batch_size = 10_000;
///
///     // Read result set as arrow batches. Infer Arrow types automatically using the meta
///     // information of `cursor`.
///     let arrow_record_batches = OdbcReader::new(cursor, max_batch_size)?;
///
///     for batch in arrow_record_batches {
///         // ... process batch ...
///     }
///     Ok(())
/// }
/// ```
pub struct OdbcReader<C: Cursor> {
    /// Converts the content of ODBC buffers into Arrow record batches
    converter: ToRecordBatch,
    /// Fetches values from the ODBC datasource using columnar batches. Values are streamed batch
    /// by batch in order to avoid reallocation of the buffers used for tranistion.
    batch_stream: BlockCursor<C, ColumnarAnyBuffer>,
}

impl<C: Cursor> OdbcReader<C> {
    /// Construct a new `OdbcReader` instance. This constructor infers the Arrow schema from the
    /// metadata of the cursor. If you want to set it explicitly use [`Self::with_arrow_schema`].
    ///
    /// # Parameters
    ///
    /// * `cursor`: ODBC cursor used to fetch batches from the data source. The constructor will
    ///   bind buffers to this cursor in order to perform bulk fetches from the source. This is
    ///   usually faster than fetching results row by row as it saves roundtrips to the database.
    ///   The type of these buffers will be inferred from the arrow schema. Not every arrow type is
    ///   supported though.
    /// * `max_batch_size`: Maximum batch size requested from the datasource.
    pub fn new(mut cursor: C, max_batch_size: usize) -> Result<Self, Error> {
        // Get number of columns from result set. We know it to contain at least one column,
        // otherwise it would not have been created.
        let schema = Arc::new(arrow_schema_from(&mut cursor)?);
        Self::with_arrow_schema(cursor, max_batch_size, schema)
    }

    /// Construct a new `OdbcReader instance.
    ///
    /// # Parameters
    ///
    /// * `cursor`: ODBC cursor used to fetch batches from the data source. The constructor will
    ///   bind buffers to this cursor in order to perform bulk fetches from the source. This is
    ///   usually faster than fetching results row by row as it saves roundtrips to the database.
    ///   The type of these buffers will be inferred from the arrow schema. Not every arrow type is
    ///   supported though.
    /// * `max_batch_size`: Maximum batch size requested from the datasource.
    /// * `schema`: Arrow schema. Describes the type of the Arrow Arrays in the record batches, but
    ///    is also used to determine CData type requested from the data source.
    pub fn with_arrow_schema(
        cursor: C,
        max_batch_size: usize,
        schema: SchemaRef,
    ) -> Result<Self, Error> {
        Self::with(
            cursor,
            max_batch_size,
            Some(schema),
            BufferAllocationOptions::default(),
        )
    }

    /// Construct a new [`crate::OdbcReader`] instance. This method allows you full control over
    /// what options to explicitly specify, and what options you want to leave to this crate to
    /// automatically decide.
    ///
    /// # Parameters
    ///
    /// * `cursor`: ODBC cursor used to fetch batches from the data source. The constructor will
    ///   bind buffers to this cursor in order to perform bulk fetches from the source. This is
    ///   usually faster than fetching results row by row as it saves roundtrips to the database.
    ///   The type of these buffers will be inferred from the arrow schema. Not every arrow type is
    ///   supported though.
    /// * `max_batch_size`: Maximum batch size requested from the datasource.
    /// * `schema`: Arrow schema. Describes the type of the Arrow Arrays in the record batches, but
    ///    is also used to determine CData type requested from the data source. Set to `None` to
    ///    infer schema from the data source.
    /// * `buffer_allocation_options`: Allows you to specify upper limits for binary and / or text
    ///    buffer types. This is useful support fetching data from e.g. VARCHAR(max) or
    ///    VARBINARY(max) columns, which otherwise might lead to errors, due to the ODBC driver
    ///    having a hard time specifying a good upper bound for the largest possible expected value.
    pub fn with(
        mut cursor: C,
        max_batch_size: usize,
        schema: Option<SchemaRef>,
        buffer_allocation_options: BufferAllocationOptions,
    ) -> Result<Self, Error> {
        let converter = ToRecordBatch::new(&mut cursor, schema.clone(), buffer_allocation_options)?;

        let row_set_buffer = converter.allocate_buffer(
            max_batch_size,
            buffer_allocation_options.fallibale_allocations,
        )?;
        let batch_stream = cursor.bind_buffer(row_set_buffer).unwrap();

        Ok(Self {
            converter,
            batch_stream,
        })
    }

    /// Consume this instance to create a similar ODBC reader which fetches batches asynchronously.
    pub fn into_concurrent(self) -> Result<ConcurrentOdbcReader<C>, Error> where C: Send + 'static {
        let cursor = self.into_cursor().expect("TODO: optimize unbind away");
        let max_batch_size = 100; // Todo: use batch size from buffer length
        let reader = ConcurrentOdbcReader::new(cursor, max_batch_size)?;
        Ok(reader)
    }

    /// Destroy the ODBC arrow reader and yield the underlyinng cursor object.
    ///
    /// One application of this is to process more than one result set in case you executed a stored
    /// procedure.
    pub fn into_cursor(self) -> Result<C, odbc_api::Error> {
        let (cursor, _buffer) = self.batch_stream.unbind()?;
        Ok(cursor)
    }
}

impl<C> Iterator for OdbcReader<C>
where
    C: Cursor,
{
    type Item = Result<RecordBatch, ArrowError>;

    fn next(&mut self) -> Option<Self::Item> {
        next(&mut self.batch_stream, &mut self.converter)
    }
}

impl<C> RecordBatchReader for OdbcReader<C>
where
    C: Cursor,
{
    fn schema(&self) -> SchemaRef {
        self.converter.schema().clone()
    }
}

pub fn next(batch_stream: &mut impl OdbcBatchStream, converter: &mut ToRecordBatch) -> Option<Result<RecordBatch, ArrowError>> {
    match batch_stream.next() {
        // We successfully fetched a batch from the database. Try to copy it into a record batch
        // and forward errors if any.
        Ok(Some(batch)) => {
            let result_record_batch = converter
                .buffer_to_record_batch(batch)
                .map_err(|mapping_error| ArrowError::ExternalError(Box::new(mapping_error)));
            Some(result_record_batch)
        }
        // We ran out of batches in the result set. End the iterator.
        Ok(None) => None,
        // We had an error fetching the next batch from the database, let's report it as an
        // external error.
        Err(odbc_error) => Some(Err(ArrowError::ExternalError(Box::new(odbc_error)))),
    }
}
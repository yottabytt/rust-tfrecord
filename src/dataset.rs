#![cfg(feature = "dataset")]

use crate::{error::Error, markers::GenericRecord};
use async_std::{
    fs::File,
    io::BufReader,
    path::{Path, PathBuf},
};
use futures::{
    io::{AsyncReadExt, AsyncSeekExt},
    stream::{StreamExt, TryStream, TryStreamExt},
};
use std::{io::SeekFrom, mem, num::NonZeroUsize, sync::Arc};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RecordIndex {
    path: Arc<PathBuf>,
    offset: u64,
    len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DatasetInit {
    pub check_integrity: bool,
    pub max_open_files: Option<NonZeroUsize>,
    pub max_workers: Option<NonZeroUsize>,
}

impl Default for DatasetInit {
    fn default() -> Self {
        Self {
            check_integrity: true,
            max_open_files: None,
            max_workers: None,
        }
    }
}

impl DatasetInit {
    pub async fn from_paths<P>(self, paths: &[P]) -> Result<Dataset, Error>
    where
        P: AsRef<Path>,
    {
        let Self {
            check_integrity,
            max_open_files,
            max_workers,
        } = self;

        let max_open_files = max_open_files.map(|num| num.get());
        let max_workers = max_workers
            .map(|num| num.get())
            .unwrap_or_else(|| num_cpus::get());
        let open_file_semaphore = max_open_files.map(|num| Arc::new(Semaphore::new(num)));

        // build record index
        let record_indexes = {
            // spawn indexing worker per path
            let future_iter = paths
                .iter()
                .map(|path| Arc::new(path.as_ref().to_owned()))
                .map(|path| {
                    let open_file_semaphore = open_file_semaphore.clone();

                    async move {
                        // acquire open file permission
                        let permit = match open_file_semaphore {
                            Some(semaphore) => Some(Arc::new(semaphore.acquire_owned().await)),
                            None => None,
                        };

                        let index_stream = {
                            // open index stream
                            let reader = BufReader::new(File::open(&*path).await?);
                            let stream = record_index_stream(reader, check_integrity);

                            // add path to index
                            let stream = stream.map_ok(move |(offset, len)| RecordIndex {
                                path: Arc::clone(&path),
                                offset,
                                len,
                            });

                            // add semaphore permission
                            let stream = stream.map_ok(move |index| {
                                let permit_clone = permit.clone();
                                (permit_clone, index)
                            });

                            stream
                        };

                        Result::<_, Error>::Ok(index_stream)
                    }
                })
                .map(async_std::task::spawn);

            // limit workers by max_workers
            let future_stream = futures::stream::iter(future_iter).buffered(max_workers);

            // drop semaphore permission
            let indexes = future_stream
                .try_flatten()
                .map_ok(|(permit, index)| {
                    mem::drop(permit);
                    index
                })
                .try_collect::<Vec<RecordIndex>>()
                .await?;

            indexes
        };

        let dataset = Dataset {
            state: Arc::new(DatasetState {
                record_indexes,
                max_workers,
                open_file_semaphore,
            }),
            open_file: None,
        };

        Ok(dataset)
    }
}

#[derive(Debug)]
struct DatasetState {
    pub record_indexes: Vec<RecordIndex>,
    pub max_workers: usize,
    pub open_file_semaphore: Option<Arc<Semaphore>>,
}

#[derive(Debug)]
pub struct Dataset {
    state: Arc<DatasetState>,
    open_file: Option<(PathBuf, BufReader<File>, Option<OwnedSemaphorePermit>)>,
}

impl Clone for Dataset {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            open_file: None,
        }
    }
}

impl Dataset {
    pub fn num_records(&self) -> usize {
        self.state.record_indexes.len()
    }

    pub async fn get<T>(&mut self, index: usize) -> Result<Option<T>, Error>
    where
        T: GenericRecord,
    {
        // try to get record index
        let record_index = match self.state.record_indexes.get(index) {
            Some(record_index) => record_index.to_owned(),
            None => return Ok(None),
        };
        let RecordIndex { offset, len, path } = record_index;

        let reader = self.open_file(&*path).await?;
        let bytes = try_read_record_at(reader, offset, len).await?;
        let record = T::from_bytes(bytes)?;
        Ok(Some(record))
    }

    pub fn stream<T>(&self) -> impl TryStream<Ok = T, Error = Error> + Send
    where
        T: GenericRecord,
    {
        let dataset = self.clone();
        futures::stream::try_unfold((dataset, 0), |state| {
            async move {
                let (mut dataset, index) = state;
                Ok(dataset.get::<T>(index).await?.map(|record| {
                    let new_state = (dataset, index + 1);
                    (record, new_state)
                }))
            }
        })
    }

    async fn open_file<P>(&mut self, path: P) -> Result<&mut BufReader<File>, Error>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();

        // re-open file if path is distinct
        match self.open_file.take() {
            Some((opened_path, reader, permit)) if opened_path == path => {
                self.open_file = Some((opened_path, reader, permit));
            }
            args => {
                mem::drop(args); // drop previous permit and reader
                let semaphore_opt = self.state.open_file_semaphore.clone();
                let permit = match semaphore_opt {
                    Some(semaphore) => Some(semaphore.acquire_owned().await),
                    None => None,
                };
                let reader = BufReader::new(File::open(&path).await?);
                self.open_file = Some((path.to_owned(), reader, permit));
            }
        }

        Ok(&mut self.open_file.as_mut().unwrap().1)
    }
}

static_assertions::assert_impl_all!(Dataset: Send, Sync);

fn record_index_stream<R>(
    reader: R,
    check_integrity: bool,
) -> impl TryStream<Ok = (u64, usize), Error = Error>
where
    R: AsyncReadExt + AsyncSeekExt + Unpin,
{
    futures::stream::try_unfold((reader, check_integrity), |args| {
        async move {
            let (mut reader, check_integrity) = args;

            let len = match crate::io::async_::try_read_len(&mut reader, check_integrity).await? {
                Some(len) => len,
                None => return Ok(None),
            };

            let offset = reader.seek(SeekFrom::Current(0)).await?;
            crate::io::async_::try_read_record_data(&mut reader, len, check_integrity).await?;

            let index = (offset, len);
            let args = (reader, check_integrity);
            Result::<_, Error>::Ok(Some((index, args)))
        }
    })
}

async fn try_read_record_at<R>(reader: &mut R, offset: u64, len: usize) -> Result<Vec<u8>, Error>
where
    R: AsyncReadExt + AsyncSeekExt + Unpin,
{
    reader.seek(SeekFrom::Start(offset)).await?;
    let bytes = crate::io::async_::try_read_record_data(reader, len, false).await?;

    Ok(bytes)
}

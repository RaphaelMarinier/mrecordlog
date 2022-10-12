use std::ops::RangeBounds;
use std::path::Path;

use crate::error::{AppendError, CreateQueueError, DeleteQueueError, TruncateError};
use crate::mem;
use crate::record::MultiPlexedRecord;
use crate::recordlog::{ReadRecordError, RecordWriter};
use crate::rolling::RollingWriter;

pub struct MultiRecordLog {
    record_log_writer: crate::recordlog::RecordWriter<RollingWriter>,
    in_mem_queues: mem::MemQueues,
}

impl MultiRecordLog {
    /// Open the multi record log.
    pub async fn open(directory_path: &Path) -> Result<Self, ReadRecordError> {
        let rolling_reader = crate::rolling::RollingReader::open(directory_path).await?;
        let mut record_reader = crate::recordlog::RecordReader::open(rolling_reader);
        let mut in_mem_queues = crate::mem::MemQueues::default();
        loop {
            let file_number = record_reader.read().current_file().clone();
            if let Some(record) = record_reader.read_record().await? {
                match record {
                    MultiPlexedRecord::AppendRecord {
                        position,
                        queue,
                        payload,
                    } => {
                        in_mem_queues
                            .append_record(queue, &file_number, position, payload)
                            .map_err(|_| ReadRecordError::Corruption)?;
                    }
                    MultiPlexedRecord::Truncate { position, queue } => {
                        in_mem_queues.truncate(queue, position);
                    }
                    MultiPlexedRecord::Touch { queue, position } => {
                        in_mem_queues
                            .touch(queue, position, &file_number)
                            .map_err(|_| ReadRecordError::Corruption)?;
                    }
                    MultiPlexedRecord::DeleteQueue { queue, position: _ } => {
                        in_mem_queues
                            .delete_queue(queue)
                            .map_err(|_| ReadRecordError::Corruption)?;
                    }
                }
            } else {
                break;
            }
        }
        let record_log_writer: RecordWriter<RollingWriter> = record_reader.into_writer().await?;
        Ok(MultiRecordLog {
            record_log_writer,
            in_mem_queues,
        })
    }

    #[cfg(test)]
    pub fn list_file_numbers(&self) -> Vec<u64> {
        let rolling_writer = self.record_log_writer.get_underlying_wrt();
        rolling_writer.list_file_numbers()
    }

    /// Creates a new queue.
    ///
    /// Returns an error if the queue already exists.
    pub async fn create_queue(&mut self, queue: &str) -> Result<(), CreateQueueError> {
        let record = MultiPlexedRecord::Touch { queue, position: 0 };
        self.record_log_writer.write_record(record).await?;
        self.record_log_writer.flush().await?;
        self.in_mem_queues.create_queue(queue)?;
        Ok(())
    }

    pub async fn delete_queue(&mut self, queue: &str) -> Result<(), DeleteQueueError> {
        let position = self.in_mem_queues.next_position(queue)?;
        let record = MultiPlexedRecord::DeleteQueue { queue, position };
        self.record_log_writer.write_record(record).await?;
        self.record_log_writer.flush().await?;
        self.in_mem_queues.delete_queue(queue)?;
        Ok(())
    }

    pub fn queue_exists(&self, queue: &str) -> bool {
        self.in_mem_queues.contains_queue(queue)
    }

    pub fn list_queues(&self) -> impl Iterator<Item = &str> {
        self.in_mem_queues.list_queues()
    }

    /// Appends a record to the log.
    ///
    /// The local_position argument can optionally be passed to enforce nilpotence.
    /// TODO if an io Error is encounterred, the in mem queue and the record log will
    /// be in an inconsistent state.
    pub async fn append_record(
        &mut self,
        queue: &str,
        position_opt: Option<u64>,
        payload: &[u8],
    ) -> Result<Option<u64>, AppendError> {
        let next_position = self.in_mem_queues.next_position(queue)?;
        if let Some(position) = position_opt {
            if position > next_position {
                return Err(AppendError::Future);
            } else if position + 1 == next_position {
                return Ok(None);
            } else if position < next_position {
                return Err(AppendError::Past);
            }
        }
        let position = position_opt.unwrap_or(next_position);
        let file_number = self.record_log_writer.current_file().clone();
        let record = MultiPlexedRecord::AppendRecord {
            position,
            queue,
            payload,
        };
        self.record_log_writer.write_record(record).await?;
        self.record_log_writer.flush().await?;
        self.in_mem_queues
            .append_record(queue, &file_number, position, payload)?;
        Ok(Some(position))
    }

    async fn touch_empty_queues(&mut self) -> Result<(), TruncateError> {
        for (queue_id, queue) in self.in_mem_queues.empty_queue_positions() {
            let next_position = queue.next_position();
            let file_number = self.record_log_writer.current_file().clone();
            let record = MultiPlexedRecord::Touch {
                queue: queue_id,
                position: next_position,
            };
            self.record_log_writer.write_record(record).await?;
            queue.touch(&file_number, next_position)?;
        }
        Ok(())
    }

    /// Truncates the queue log.
    ///
    /// This method will always truncate the record log, and release the associated memory.
    pub async fn truncate(&mut self, queue: &str, position: u64) -> Result<(), TruncateError> {
        if position >= self.in_mem_queues.next_position(queue)? {
            return Err(TruncateError::Future);
        }
        self.in_mem_queues.truncate(queue, position);
        self.record_log_writer
            .write_record(MultiPlexedRecord::Truncate { position, queue })
            .await?;
        self.touch_empty_queues().await?;
        self.record_log_writer.flush().await?;
        self.record_log_writer.gc().await?;
        Ok(())
    }

    pub fn range<R>(
        &self,
        queue: &str,
        range: R,
    ) -> Option<impl Iterator<Item = (u64, &[u8])> + '_>
    where
        R: RangeBounds<u64> + 'static,
    {
        // We do not rely on `entry` in order to avoid
        // the allocation.
        self.in_mem_queues.range(queue, range)
    }
}

//! Bounded invocation-local stream handle registry.

use super::*;

impl FileStreamRegistry {
    pub(in crate::application_data) fn new(budgets: &ModuleDataBudgets) -> Self {
        Self {
            next_handle: 1,
            streams: BTreeMap::new(),
            max_open: budgets.max_open_streams as usize,
            max_bytes: budgets.max_stream_bytes,
            buffers: temper_wasm::StreamRegistry::new(),
        }
    }

    pub(super) fn insert(
        &mut self,
        stream: FileStream,
        bytes: Vec<u8>,
    ) -> Result<u32, ModuleDataError> {
        if self.streams.len() >= self.max_open {
            return Err(data_error(
                ModuleDataErrorKind::BudgetExceeded,
                "OpenStreamBudgetExceeded",
                "File stream budget exhausted",
            ));
        }
        let handle = self.next_handle;
        self.next_handle = self.next_handle.checked_add(1).ok_or_else(|| {
            data_error(
                ModuleDataErrorKind::BudgetExceeded,
                "StreamHandleExhausted",
                "File stream handles exhausted",
            )
        })?;
        self.buffers.register_stream(&handle.to_string(), bytes);
        self.streams.insert(handle, stream);
        Ok(handle)
    }

    pub(super) fn read(&mut self, handle: u32, max: usize) -> Result<Vec<u8>, i32> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let FileStream::Read { offset } = self.streams.get(&handle).ok_or(-3)? else {
            return Err(-3);
        };
        let offset = *offset;
        let stream_id = handle.to_string();
        let bytes = self.buffers.get_stream(&stream_id).ok_or(-3)?;
        if offset == bytes.len() {
            self.take(handle);
            return Ok(Vec::new());
        }
        let end = offset.saturating_add(max).min(bytes.len());
        if end as u64 > self.max_bytes {
            return Err(-4);
        }
        let chunk = bytes[offset..end].to_vec();
        if let Some(FileStream::Read { offset }) = self.streams.get_mut(&handle) {
            *offset = end;
        }
        Ok(chunk)
    }

    pub(super) fn write(&mut self, handle: u32, bytes: &[u8]) -> Result<usize, i32> {
        let FileStream::Write { committing, .. } = self.streams.get(&handle).ok_or(-3)? else {
            return Err(-3);
        };
        if *committing {
            return Err(-3);
        }
        self.buffers
            .append_stream_bounded(&handle.to_string(), bytes, self.max_bytes as usize)
            .ok_or(-4)
    }
}

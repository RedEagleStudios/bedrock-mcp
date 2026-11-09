//! Copying a swap chain's back buffer into CPU-readable memory on the GPU.

use std::{
    mem::ManuallyDrop,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use protocol::frames::format;
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
        Graphics::{
            Direct3D12::*,
            Dxgi::{Common::*, IDXGISwapChain, IDXGISwapChain3},
        },
        System::Threading::{CreateEventW, WaitForSingleObject},
    },
    core::Interface,
};

use super::shm::Shape;

/// Readback buffers in flight; a frame is skipped when all are busy.
pub const BUFFERS: usize = 4;
const WAIT_MS: u32 = 2000;

/// The CPU side of a copier: what the worker thread needs to read finished copies.
pub struct Readback {
    pub fence: ID3D12Fence,
    pub buffers: Vec<ID3D12Resource>,
    pub busy: [AtomicBool; BUFFERS],
    pub footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    pub total_bytes: u64,
    pub shape: Shape,
    event: HANDLE,
}

// SAFETY: fences and resources are free-threaded; the event handle is only waited on.
unsafe impl Send for Readback {}
unsafe impl Sync for Readback {}

impl Readback {
    /// Wait until the fence reaches `value`.
    pub fn wait(&self, value: u64) -> bool {
        unsafe {
            if self.fence.GetCompletedValue() >= value {
                return true;
            }
            self.fence.SetEventOnCompletion(value, self.event).is_ok()
                && WaitForSingleObject(self.event, WAIT_MS) == WAIT_OBJECT_0
        }
    }
}

impl Drop for Readback {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.event);
        }
    }
}

/// A copy queued on the GPU: the buffer it lands in and the fence value that marks it done.
pub struct Queued {
    pub readback: Arc<Readback>,
    pub buffer: usize,
    pub fence_value: u64,
}

/// The GPU side, used only on the thread that presents.
pub struct Copier {
    swapchain: usize,
    desc: (u64, u32, DXGI_FORMAT),
    queue: ID3D12CommandQueue,
    allocators: Vec<ID3D12CommandAllocator>,
    lists: Vec<ID3D12GraphicsCommandList>,
    fence_value: u64,
    next: usize,
    pub readback: Arc<Readback>,
}

fn shape_format(format: DXGI_FORMAT) -> Option<u32> {
    match format {
        DXGI_FORMAT_B8G8R8A8_UNORM
        | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB
        | DXGI_FORMAT_B8G8R8X8_UNORM => Some(format::BGRA8),
        DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_R8G8B8A8_UNORM_SRGB => Some(format::RGBA8),
        DXGI_FORMAT_R10G10B10A2_UNORM => Some(format::RGB10A2),
        _ => None,
    }
}

/// A borrowed resource pointer for barrier and copy descriptions, which do not own it.
fn borrow(resource: &ID3D12Resource) -> ManuallyDrop<Option<ID3D12Resource>> {
    // SAFETY: same layout; ManuallyDrop keeps the description from releasing it.
    unsafe { std::mem::transmute_copy(resource) }
}

fn transition(
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: borrow(resource),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

fn error(what: &str, error: windows::core::Error) -> String {
    format!("{what}: {error}")
}

impl Copier {
    /// Whether this copier fits `swapchain`'s current back buffer `desc`.
    pub fn fits(&self, swapchain: usize, desc: &D3D12_RESOURCE_DESC) -> bool {
        self.swapchain == swapchain && self.desc == (desc.Width, desc.Height, desc.Format)
    }

    /// Resources for copying `buffer`, a back buffer of `swapchain`, on the queue `queue`.
    pub fn new(
        swapchain: usize,
        queue: ID3D12CommandQueue,
        buffer: &ID3D12Resource,
    ) -> Result<Self, String> {
        unsafe {
            let desc = buffer.GetDesc();
            let format = shape_format(desc.Format)
                .ok_or_else(|| format!("unsupported back buffer format {}", desc.Format.0))?;
            let mut device: Option<ID3D12Device> = None;
            buffer
                .GetDevice(&mut device)
                .map_err(|e| error("GetDevice", e))?;
            let device = device.ok_or("the back buffer has no device")?;
            let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
            let mut total_bytes = 0u64;
            device.GetCopyableFootprints(
                &desc,
                0,
                1,
                0,
                Some(&mut footprint),
                None,
                None,
                Some(&mut total_bytes),
            );
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_READBACK,
                ..Default::default()
            };
            let buffer_desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: total_bytes,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let mut buffers = Vec::with_capacity(BUFFERS);
            let mut allocators = Vec::with_capacity(BUFFERS);
            let mut lists = Vec::with_capacity(BUFFERS);
            for _ in 0..BUFFERS {
                let mut readback: Option<ID3D12Resource> = None;
                device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &buffer_desc,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                        None,
                        &mut readback,
                    )
                    .map_err(|e| error("CreateCommittedResource", e))?;
                buffers.push(readback.ok_or("no readback buffer")?);
                let allocator: ID3D12CommandAllocator = device
                    .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                    .map_err(|e| error("CreateCommandAllocator", e))?;
                let list: ID3D12GraphicsCommandList = device
                    .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                    .map_err(|e| error("CreateCommandList", e))?;
                list.Close().map_err(|e| error("Close", e))?;
                allocators.push(allocator);
                lists.push(list);
            }
            let fence: ID3D12Fence = device
                .CreateFence(0, D3D12_FENCE_FLAG_NONE)
                .map_err(|e| error("CreateFence", e))?;
            let event =
                CreateEventW(None, false, false, None).map_err(|e| error("CreateEventW", e))?;
            Ok(Self {
                swapchain,
                desc: (desc.Width, desc.Height, desc.Format),
                queue,
                allocators,
                lists,
                fence_value: 0,
                next: 0,
                readback: Arc::new(Readback {
                    fence,
                    buffers,
                    busy: [const { AtomicBool::new(false) }; BUFFERS],
                    footprint,
                    total_bytes,
                    shape: Shape {
                        width: desc.Width as u32,
                        height: desc.Height,
                        format,
                    },
                    event,
                }),
            })
        }
    }

    /// Queue a copy of `buffer` before the present; `None` when every readback buffer is busy.
    pub fn copy(&mut self, buffer: &ID3D12Resource) -> Result<Option<Queued>, String> {
        let index = self.next;
        if self.readback.busy[index].load(Ordering::Acquire) {
            return Ok(None);
        }
        self.next = (index + 1) % BUFFERS;
        let readback = &self.readback;
        unsafe {
            let allocator = &self.allocators[index];
            let list = &self.lists[index];
            allocator.Reset().map_err(|e| error("allocator Reset", e))?;
            list.Reset(allocator, None)
                .map_err(|e| error("list Reset", e))?;
            list.ResourceBarrier(&[transition(
                buffer,
                D3D12_RESOURCE_STATE_PRESENT,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            let destination = D3D12_TEXTURE_COPY_LOCATION {
                pResource: borrow(&readback.buffers[index]),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: readback.footprint,
                },
            };
            let source = D3D12_TEXTURE_COPY_LOCATION {
                pResource: borrow(buffer),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: 0,
                },
            };
            list.CopyTextureRegion(&destination, 0, 0, 0, &source, None);
            list.ResourceBarrier(&[transition(
                buffer,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_PRESENT,
            )]);
            list.Close().map_err(|e| error("list Close", e))?;
            let submit: ID3D12CommandList = list.cast().map_err(|e| error("cast", e))?;
            self.queue.ExecuteCommandLists(&[Some(submit)]);
            self.fence_value += 1;
            self.queue
                .Signal(&readback.fence, self.fence_value)
                .map_err(|e| error("Signal", e))?;
        }
        readback.busy[index].store(true, Ordering::Release);
        Ok(Some(Queued {
            readback: Arc::clone(readback),
            buffer: index,
            fence_value: self.fence_value,
        }))
    }
}

impl Drop for Copier {
    fn drop(&mut self) {
        // Command allocators must outlive the work recorded into them.
        self.readback.wait(self.fence_value);
    }
}

/// The queue that presents `swapchain`'s images: D3D12 swap chains report it as their device.
pub fn present_queue(swapchain: &IDXGISwapChain) -> Option<ID3D12CommandQueue> {
    unsafe { swapchain.GetDevice::<ID3D12CommandQueue>() }.ok()
}

/// `swapchain`'s current back buffer.
pub fn back_buffer(swapchain: &IDXGISwapChain) -> Result<ID3D12Resource, String> {
    unsafe {
        let swapchain: IDXGISwapChain3 =
            swapchain.cast().map_err(|e| error("IDXGISwapChain3", e))?;
        let index = swapchain.GetCurrentBackBufferIndex();
        swapchain
            .GetBuffer(index)
            .map_err(|e| error("GetBuffer", e))
    }
}

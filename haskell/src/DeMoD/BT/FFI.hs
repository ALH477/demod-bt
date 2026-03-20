-- | DeMoD.BT.FFI - Foreign Function Interface to Rust Data Plane
--
-- Imports the C ABI functions exported by the Rust @libdemod_bt@ library.
-- All imports use @ccall unsafe@ to skip GHC's GC synchronization,
-- achieving ~2.4ns per call overhead. This is safe because the Rust
-- functions do not call back into Haskell and complete quickly.
--
-- The full lifecycle from Haskell:
--   initPipeline -> registerEndpoints -> (poll events in loop) ->
--   acquireAndStart -> (streaming) -> stopStream -> shutdownPipeline
--
-- LGPL-3.0 | Patent Pending | (c) 2025 DeMoD LLC
module DeMoD.BT.FFI
  ( -- * Pipeline Lifecycle
    initPipeline
  , registerEndpoints
  , acquireAndStart
  , startStream
  , stopStream
  , isStreaming
  , shutdownPipeline

    -- * Volume Control
  , setVolume
  , getVolume

    -- * Event Polling
  , EventType (..)
  , FfiEvent (..)
  , pollEvent

    -- * Metrics
  , MetricsSnapshot (..)
  , getMetrics

    -- * DCF Constants
  , dcfHeaderSize
  , dcfOptimalPayload

    -- * Info
  , getVersion
  , getStatus
  ) where

import Foreign
import Foreign.C.Types
import Foreign.C.String
import Data.Word (Word8, Word16, Word32)
import System.IO.Unsafe (unsafePerformIO)

import DeMoD.BT.Types (StreamDirection (..))

-- ═══════════════════════════════════════════════════════════════════
-- Event Types
-- ═══════════════════════════════════════════════════════════════════

-- | Event types returned by the Rust runtime when polling.
-- These correspond to the DEMOD_BT_EVT_* constants in ffi.h.
data EventType
  = EvtNone               -- ^ No event pending
  | EvtDeviceConnected    -- ^ A Bluetooth device connected
  | EvtDeviceDisconnected -- ^ A Bluetooth device disconnected
  | EvtTransportAcquired  -- ^ BlueZ transport fd is ready
  | EvtTransportReleased  -- ^ BlueZ transport was released
  | EvtCodecNegotiated    -- ^ Codec config was agreed upon
  | EvtError              -- ^ An error occurred
  deriving stock (Show, Eq)

-- | Convert from C int to EventType.
fromEventCode :: CInt -> EventType
fromEventCode 0  = EvtNone
fromEventCode 1  = EvtDeviceConnected
fromEventCode 2  = EvtDeviceDisconnected
fromEventCode 3  = EvtTransportAcquired
fromEventCode 4  = EvtTransportReleased
fromEventCode 5  = EvtCodecNegotiated
fromEventCode _  = EvtError

-- ═══════════════════════════════════════════════════════════════════
-- FFI Structs (must match Rust repr(C) layout exactly)
-- ═══════════════════════════════════════════════════════════════════

-- | Runtime metrics from the Rust audio pipeline.
data MetricsSnapshot = MetricsSnapshot
  { msFramesProcessed :: !Word32
  , msUnderruns       :: !Word32
  , msOverruns        :: !Word32
  , msBufferLevel     :: !Word32
  , msRunning         :: !Word8
  } deriving stock (Show)

instance Storable MetricsSnapshot where
  sizeOf    _ = 20  -- 4*4 + 1 + 3 padding = 20 (Rust repr(C) alignment)
  alignment _ = 4
  peek ptr = MetricsSnapshot
    <$> peekByteOff ptr 0
    <*> peekByteOff ptr 4
    <*> peekByteOff ptr 8
    <*> peekByteOff ptr 12
    <*> peekByteOff ptr 16
  poke ptr MetricsSnapshot{..} = do
    pokeByteOff ptr 0  msFramesProcessed
    pokeByteOff ptr 4  msUnderruns
    pokeByteOff ptr 8  msOverruns
    pokeByteOff ptr 12 msBufferLevel
    pokeByteOff ptr 16 msRunning

-- | Event data from the Rust runtime.
data FfiEvent = FfiEvent
  { feEventType  :: !EventType
  , feFd         :: !CInt         -- ^ BT transport fd (for TransportAcquired)
  , feReadMtu    :: !CUInt
  , feWriteMtu   :: !CUInt
  , feStringData :: !(Maybe String) -- ^ Address, path, or error message
  } deriving stock (Show)

-- | Raw C struct for FfiEvent. We peek this then convert.
data RawFfiEvent = RawFfiEvent
  { rfeEventType  :: !CInt
  , rfeFd         :: !CInt
  , rfeReadMtu    :: !CUInt
  , rfeWriteMtu   :: !CUInt
  , rfeStringData :: !CString
  }

instance Storable RawFfiEvent where
  -- On x86_64: 4+4+4+4+8 = 24 bytes (no internal padding needed)
  -- On aarch64: same layout (LP64)
  -- On 32-bit: 4+4+4+4+4 = 20 bytes
  sizeOf    _ = 4 + 4 + 4 + 4 + sizeOf (undefined :: CString)
  alignment _ = alignment (undefined :: CString) -- pointer alignment (8 on 64-bit)
  peek ptr = RawFfiEvent
    <$> peekByteOff ptr 0   -- event_type: c_int at offset 0
    <*> peekByteOff ptr 4   -- fd: c_int at offset 4
    <*> peekByteOff ptr 8   -- read_mtu: c_uint at offset 8
    <*> peekByteOff ptr 12  -- write_mtu: c_uint at offset 12
    <*> peekByteOff ptr 16  -- string_data: pointer at offset 16
  poke ptr RawFfiEvent{..} = do
    pokeByteOff ptr 0  rfeEventType
    pokeByteOff ptr 4  rfeFd
    pokeByteOff ptr 8  rfeReadMtu
    pokeByteOff ptr 12 rfeWriteMtu
    pokeByteOff ptr 16 rfeStringData

-- ═══════════════════════════════════════════════════════════════════
-- Raw FFI Imports
-- ═══════════════════════════════════════════════════════════════════

foreign import ccall unsafe "demod_bt_init"
  c_init :: CUInt -> CUInt -> CInt -> CUInt -> CUInt -> IO CInt

foreign import ccall unsafe "demod_bt_register"
  c_register :: IO CInt

foreign import ccall unsafe "demod_bt_acquire_and_start"
  c_acquire_and_start :: CString -> IO CInt

foreign import ccall unsafe "demod_bt_start_stream"
  c_start_stream :: CInt -> Ptr Word8 -> CUInt -> IO CInt

foreign import ccall unsafe "demod_bt_stop_stream"
  c_stop_stream :: IO ()

foreign import ccall unsafe "demod_bt_is_streaming"
  c_is_streaming :: IO CInt

foreign import ccall unsafe "demod_bt_set_volume"
  c_set_volume :: CUInt -> IO CInt

foreign import ccall unsafe "demod_bt_get_volume"
  c_get_volume :: IO CInt

foreign import ccall unsafe "demod_bt_shutdown"
  c_shutdown :: IO ()

foreign import ccall unsafe "demod_bt_poll_event"
  c_poll_event :: Ptr RawFfiEvent -> IO CInt

foreign import ccall unsafe "demod_bt_get_metrics"
  c_get_metrics :: Ptr MetricsSnapshot -> IO CInt

foreign import ccall unsafe "demod_bt_dcf_header_size"
  c_dcf_header_size :: CUInt

foreign import ccall unsafe "demod_bt_dcf_optimal_payload"
  c_dcf_optimal_payload :: CUInt

foreign import ccall unsafe "demod_bt_version"
  c_version :: CString

foreign import ccall unsafe "demod_bt_status"
  c_status :: IO CString

foreign import ccall unsafe "demod_bt_free_string"
  c_free_string :: CString -> IO ()

-- ═══════════════════════════════════════════════════════════════════
-- Haskell Wrappers
-- ═══════════════════════════════════════════════════════════════════

-- | Initialize the Rust runtime and audio pipeline.
initPipeline
  :: Word32          -- ^ Sample rate (e.g., 44100)
  -> Word32          -- ^ Channels (1 or 2)
  -> StreamDirection -- ^ Sink or Source
  -> Word32          -- ^ Jitter buffer depth in ms
  -> Word32          -- ^ DCF payload size (239 for optimal)
  -> IO (Either String ())
initPipeline sr ch dir jitter payload = do
  let dirInt = case dir of { Sink -> 0; Source -> 1 }
  result <- c_init
    (fromIntegral sr) (fromIntegral ch) dirInt
    (fromIntegral jitter) (fromIntegral payload)
  pure $ if result == 0 then Right () else Left "demod_bt_init failed"

-- | Register A2DP endpoints with BlueZ. Must be called after initPipeline.
-- After this call, we are visible as a Bluetooth audio device.
registerEndpoints :: IO (Either String ())
registerEndpoints = do
  result <- c_register
  pure $ if result == 0 then Right ()
    else Left "demod_bt_register failed (is bluetoothd running?)"

-- | Acquire a BlueZ transport and start streaming. All-in-one call.
-- The transport_path comes from a TransportAcquired event.
acquireAndStart :: String -> IO (Either String ())
acquireAndStart path = withCString path $ \cpath -> do
  result <- c_acquire_and_start cpath
  pure $ if result == 0 then Right ()
    else Left "demod_bt_acquire_and_start failed"

-- | Start streaming with a raw fd and codec config. Lower-level API.
startStream :: Int -> [Word8] -> IO (Either String ())
startStream fd codecConfig =
  withArrayLen codecConfig $ \len ptr -> do
    result <- c_start_stream (fromIntegral fd) ptr (fromIntegral len)
    pure $ if result == 0 then Right ()
      else Left "demod_bt_start_stream failed"

-- | Stop the audio engine. Keeps BlueZ registration alive for reconnection.
stopStream :: IO ()
stopStream = c_stop_stream

-- | Check if audio is currently streaming.
isStreaming :: IO Bool
isStreaming = do
  result <- c_is_streaming
  pure (result == 1)

-- | [1.3] Set the audio output volume (0-127, AVRCP scale).
-- This propagates to the Rust engine's atomic volume, which the
-- CPAL audio callback reads on each buffer fill. No locks, no
-- allocation, no syscall -- just an atomic store.
setVolume :: Int -> IO ()
setVolume vol = do
  _ <- c_set_volume (fromIntegral (max 0 (min 127 vol)))
  pure ()

-- | [1.3] Get the current volume level (0-127).
getVolume :: IO Int
getVolume = fromIntegral <$> c_get_volume

-- | Shut down the Rust runtime completely. Call on exit.
shutdownPipeline :: IO ()
shutdownPipeline = c_shutdown

-- | Poll for the next event from the Rust runtime (non-blocking).
-- Returns Nothing if no events are pending. The caller should
-- poll in a loop with a small sleep between iterations.
pollEvent :: IO (Maybe FfiEvent)
pollEvent = alloca $ \ptr -> do
  code <- c_poll_event ptr
  if code == 0 -- EVT_NONE
    then pure Nothing
    else do
      raw <- peek ptr
      -- Extract string data if present, then free the C string
      mStr <- if rfeStringData raw == nullPtr
        then pure Nothing
        else do
          s <- peekCString (rfeStringData raw)
          c_free_string (rfeStringData raw)
          pure (Just s)
      pure $ Just FfiEvent
        { feEventType  = fromEventCode (rfeEventType raw)
        , feFd         = rfeFd raw
        , feReadMtu    = rfeReadMtu raw
        , feWriteMtu   = rfeWriteMtu raw
        , feStringData = mStr
        }

-- | Read current stream metrics from the Rust pipeline.
getMetrics :: IO (Maybe MetricsSnapshot)
getMetrics = alloca $ \ptr -> do
  result <- c_get_metrics ptr
  if result == 0
    then Just <$> peek ptr
    else pure Nothing

-- | DCF header size constant (17 bytes).
dcfHeaderSize :: Word32
dcfHeaderSize = fromIntegral c_dcf_header_size

-- | Optimal DCF payload size (239 bytes, for 256-byte total packets).
dcfOptimalPayload :: Word32
dcfOptimalPayload = fromIntegral c_dcf_optimal_payload

-- | Library version string.
getVersion :: String
getVersion = unsafePerformIO $ peekCString c_version
{-# NOINLINE getVersion #-}

-- | Human-readable status string.
getStatus :: IO String
getStatus = do
  cstr <- c_status
  if cstr == nullPtr
    then pure "ERROR: null status"
    else do
      str <- peekCString cstr
      c_free_string cstr
      pure str

-- | DeMoD.BT.DCF - DeMoD Communications Framework Protocol Logic
--
-- Haskell-side DCF frame construction for control messages
-- (metadata, volume, device info). Audio frame packetization
-- is handled by the Rust data plane for performance.
--
-- The 17-byte header maps to Z/136Z in the mathematical formalization.
-- The 239-byte optimal payload produces 256-byte total packets
-- (power-of-2 aligned, matching native Bluetooth A2DP overhead).
--
-- LGPL-3.0 | Patent Pending | (c) 2025 DeMoD LLC
module DeMoD.BT.DCF
  ( -- * Frame Construction
    buildMetadataFrame
  , buildVolumeFrame
  , buildHeartbeat

    -- * Frame Parsing
  , parseHeader
  , DcfHeader (..)

    -- * Constants (re-exported from Rust via FFI)
  , headerSize
  , optimalPayload
  ) where

import Data.ByteString (ByteString)
import qualified Data.ByteString as BS
import Data.ByteString.Builder
import qualified Data.ByteString.Lazy as LBS
import Data.Text (Text)
import Data.Text.Encoding (encodeUtf8)
import Data.Word (Word8, Word32, Word64)
import Data.Binary.Get
import Data.Time.Clock.POSIX (getPOSIXTime)

import DeMoD.BT.FFI (dcfHeaderSize, dcfOptimalPayload)

-- | DCF header size (17 bytes). From Rust via FFI.
headerSize :: Word32
headerSize = dcfHeaderSize

-- | Optimal payload size (239 bytes). From Rust via FFI.
optimalPayload :: Word32
optimalPayload = dcfOptimalPayload

-- | Parsed DCF header.
data DcfHeader = DcfHeader
  { dhMsgType    :: !Word8
  , dhSequence   :: !Word32
  , dhTimestamp   :: !Word64
  , dhPayloadLen :: !Word32
  } deriving stock (Show, Eq)

-- | Message type constants (must match Rust MessageType repr).
pattern MsgHeartbeat, MsgTrackMetadata, MsgVolumeChange :: Word8
pattern MsgHeartbeat     = 0x01
pattern MsgTrackMetadata = 0x30
pattern MsgVolumeChange  = 0x21

-- ═══════════════════════════════════════════════════════════════════
-- Frame Construction
-- ═══════════════════════════════════════════════════════════════════

-- | Get current timestamp in microseconds since epoch.
currentTimestamp :: IO Word64
currentTimestamp = do
  t <- getPOSIXTime
  pure $ round (t * 1_000_000)

-- | Build the 17-byte DCF header as a strict ByteString.
buildHeader :: Word8 -> Word32 -> Word64 -> Word32 -> ByteString
buildHeader msgType seqNum ts payloadLen =
  LBS.toStrict $ toLazyByteString $
    word8 msgType
    <> word32BE seqNum
    <> word64BE ts
    <> word32BE payloadLen

-- | Build a track metadata DCF frame (type 0x30).
-- Payload: UTF-8 encoded "title\0artist\0album\0"
buildMetadataFrame :: Word32 -> Text -> Text -> Text -> IO ByteString
buildMetadataFrame seqNum title artist album = do
  ts <- currentTimestamp
  let payload = encodeUtf8 title <> "\0"
             <> encodeUtf8 artist <> "\0"
             <> encodeUtf8 album <> "\0"
      hdr = buildHeader MsgTrackMetadata seqNum ts (fromIntegral $ BS.length payload)
  pure $ hdr <> payload

-- | Build a volume change DCF frame (type 0x21).
-- Payload: 2 bytes, big-endian volume level (0-127).
buildVolumeFrame :: Word32 -> Word8 -> IO ByteString
buildVolumeFrame seqNum volume = do
  ts <- currentTimestamp
  let payload = LBS.toStrict $ toLazyByteString $ word8 volume <> word8 0
      hdr = buildHeader MsgVolumeChange seqNum ts 2
  pure $ hdr <> payload

-- | Build a heartbeat DCF frame (type 0x01). Empty payload.
buildHeartbeat :: Word32 -> IO ByteString
buildHeartbeat seqNum = do
  ts <- currentTimestamp
  pure $ buildHeader MsgHeartbeat seqNum ts 0

-- ═══════════════════════════════════════════════════════════════════
-- Frame Parsing
-- ═══════════════════════════════════════════════════════════════════

-- | Parse a DCF header from the first 17 bytes of a ByteString.
parseHeader :: ByteString -> Either String DcfHeader
parseHeader bs
  | BS.length bs < 17 = Left "DCF header too short (need 17 bytes)"
  | otherwise = case runGetOrFail getHeader (LBS.fromStrict bs) of
      Left (_, _, err) -> Left err
      Right (_, _, hdr) -> Right hdr
  where
    getHeader = DcfHeader
      <$> getWord8
      <*> getWord32be
      <*> getWord64be
      <*> getWord32be

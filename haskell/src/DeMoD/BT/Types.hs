-- | DeMoD.BT.Types - Shared type definitions for the BT control plane.
--
-- These types are used across all control plane modules (AVDTP, AVRCP,
-- BlueZ, DCF) and represent the protocol-level abstractions that
-- Haskell's type system enforces at compile time.
--
-- LGPL-3.0 | Patent Pending | (c) 2025 DeMoD LLC
module DeMoD.BT.Types
  ( -- * Device Types
    BTAddress (..)
  , DeviceInfo (..)
  , DeviceClass (..)

    -- * Audio Configuration
  , AudioCodec (..)
  , CodecCapabilities (..)
  , StreamDirection (..)
  , SampleRate (..)

    -- * Transport
  , TransportPath
  , TransportState (..)
  , TransportInfo (..)

    -- * AVRCP Metadata
  , TrackMetadata (..)
  , PlaybackState (..)
  , PlaybackStatus (..)

    -- * Events
  , BTEvent (..)
  ) where

import Data.Text (Text)
import Data.ByteString (ByteString)
import Data.Word (Word8, Word16, Word32, Word64)
import GHC.Generics (Generic)

-- ═══════════════════════════════════════════════════════════════════
-- Device Types
-- ═══════════════════════════════════════════════════════════════════

-- | Bluetooth MAC address (e.g., "AA:BB:CC:DD:EE:FF").
newtype BTAddress = BTAddress { unBTAddress :: Text }
  deriving stock (Eq, Ord, Show, Generic)

-- | Information about a connected Bluetooth device.
data DeviceInfo = DeviceInfo
  { diAddress    :: !BTAddress
  , diName       :: !Text
  , diClass      :: !DeviceClass
  , diPaired     :: !Bool
  , diConnected  :: !Bool
  , diTrusted    :: !Bool
  } deriving stock (Show, Generic)

-- | Major device class (for display and routing decisions).
data DeviceClass
  = ClassHeadphones
  | ClassSpeaker
  | ClassCarAudio
  | ClassPhone
  | ClassComputer
  | ClassUnknown !Word32
  deriving stock (Show, Eq, Generic)

-- ═══════════════════════════════════════════════════════════════════
-- Audio Configuration
-- ═══════════════════════════════════════════════════════════════════

-- | Supported audio codecs (matches Rust AudioCodec repr).
data AudioCodec
  = CodecSBC      -- ^ Mandatory for A2DP Classic
  | CodecAAC      -- ^ MPEG-2/4 AAC
  | CodecAptX     -- ^ Qualcomm aptX (proprietary)
  | CodecLDAC     -- ^ Sony LDAC (high-res)
  | CodecLC3      -- ^ LE Audio (Bluetooth 5.2+)
  deriving stock (Show, Eq, Ord, Enum, Bounded, Generic)

-- | Raw codec capabilities blob (from BlueZ negotiation).
data CodecCapabilities = CodecCapabilities
  { ccCodec        :: !AudioCodec
  , ccRawBytes     :: !ByteString  -- ^ Raw capability bytes
  , ccSampleRates  :: ![SampleRate]
  , ccChannels     :: !Word8       -- ^ 1 = mono, 2 = stereo
  , ccBitpool      :: !(Word8, Word8) -- ^ (min, max) for SBC
  } deriving stock (Show, Generic)

-- | Whether we are receiving or sending audio.
data StreamDirection = Sink | Source
  deriving stock (Show, Eq, Ord, Enum, Bounded, Generic)

-- | Supported sample rates.
data SampleRate = SR16000 | SR32000 | SR44100 | SR48000 | SR96000
  deriving stock (Show, Eq, Ord, Enum, Bounded, Generic)

-- | Convert SampleRate to Hz.
sampleRateHz :: SampleRate -> Word32
sampleRateHz = \case
  SR16000 -> 16000
  SR32000 -> 32000
  SR44100 -> 44100
  SR48000 -> 48000
  SR96000 -> 96000

-- ═══════════════════════════════════════════════════════════════════
-- Transport
-- ═══════════════════════════════════════════════════════════════════

-- | D-Bus object path for a BlueZ transport (e.g., "/org/bluez/hci0/dev_.../fd0").
type TransportPath = Text

-- | AVDTP transport state.
-- This maps directly to the AVDTP state machine states (see AVDTP module).
data TransportState
  = TSIdle        -- ^ No transport exists
  | TSPending     -- ^ Transport created, not yet acquired
  | TSActive      -- ^ Transport acquired, audio flowing
  deriving stock (Show, Eq, Ord, Enum, Bounded, Generic)

-- | Information about an active transport.
data TransportInfo = TransportInfo
  { tiPath          :: !TransportPath
  , tiState         :: !TransportState
  , tiCodec         :: !AudioCodec
  , tiConfiguration :: !ByteString
  , tiVolume        :: !Word16       -- ^ 0-127 (AVRCP absolute volume)
  } deriving stock (Show, Generic)

-- ═══════════════════════════════════════════════════════════════════
-- AVRCP Metadata
-- ═══════════════════════════════════════════════════════════════════

-- | Track metadata synchronized via AVRCP 1.3+.
data TrackMetadata = TrackMetadata
  { tmTitle    :: !Text
  , tmArtist   :: !Text
  , tmAlbum    :: !Text
  , tmGenre    :: !Text
  , tmTrackNum :: !Word32
  , tmDuration :: !Word64  -- ^ Microseconds
  } deriving stock (Show, Generic)

-- | Playback state synchronized via AVRCP.
data PlaybackState = PlaybackState
  { psStatus   :: !PlaybackStatus
  , psPosition :: !Word64  -- ^ Current position in microseconds
  , psShuffle  :: !Bool
  , psRepeat   :: !Bool
  } deriving stock (Show, Generic)

-- | AVRCP playback status.
data PlaybackStatus
  = Stopped
  | Playing
  | Paused
  | FastForward
  | Rewind
  | Error
  deriving stock (Show, Eq, Ord, Enum, Bounded, Generic)

-- ═══════════════════════════════════════════════════════════════════
-- Events
-- ═══════════════════════════════════════════════════════════════════

-- | Events flowing from the BlueZ/AVRCP layer to the application.
-- These are pushed to a TBQueue and consumed by the main event loop.
data BTEvent
  = EvDeviceConnected    !DeviceInfo
  | EvDeviceDisconnected !BTAddress
  | EvTransportCreated   !TransportInfo
  | EvTransportAcquired  !TransportPath
  | EvTransportReleased  !TransportPath
  | EvCodecNegotiated    !AudioCodec !ByteString
  | EvTrackChanged       !TrackMetadata
  | EvPlaybackChanged    !PlaybackState
  | EvVolumeChanged      !Word16
  | EvError              !Text
  deriving stock (Show, Generic)

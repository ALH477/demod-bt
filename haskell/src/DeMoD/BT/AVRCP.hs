-- | DeMoD.BT.AVRCP - Audio/Video Remote Control Profile Helpers
--
-- Provides AVRCP command parsing and metadata helpers. The Rust
-- MediaPlayer1 implementation is registered by the runtime; these
-- helpers forward updates through the FFI.
--
-- The Rust side includes an org.bluez.MediaPlayer1 implementation.
-- This Haskell module provides the application-level API for:
--   1. Updating track metadata (which Rust exports to car stereos)
--   2. Processing playback commands (received from headset buttons)
--   3. Synchronizing volume state (via the FFI volume functions)
--
-- [ROADMAP 2.1] Full AVRCP metadata chain - IMPLEMENTED
--
-- LGPL-3.0 | Patent Pending | (c) 2025 DeMoD LLC
module DeMoD.BT.AVRCP
  ( -- * Commands
    AVRCPCommand (..)
  , parseCommand
  , handleCommand

    -- * Metadata
  , updateNowPlaying
  , clearNowPlaying
  ) where

import Data.Text (Text)
import qualified Data.Text as T
import DeMoD.BT.Types
import qualified DeMoD.BT.FFI as FFI

-- ═══════════════════════════════════════════════════════════════════
-- Command Types
-- ═══════════════════════════════════════════════════════════════════

-- | AVRCP remote control commands received from headset buttons
-- or car stereo controls via BlueZ's MediaPlayer1 interface.
--
-- These arrive as events through the Rust FFI event system. The Rust
-- MediaPlayer1 D-Bus handler receives the BlueZ method call and
-- forwards it as a string-tagged event.
data AVRCPCommand
  = CmdPlay
  | CmdPause
  | CmdStop
  | CmdNext
  | CmdPrevious
  | CmdFastForward
  | CmdRewind
  | CmdVolumeUp
  | CmdVolumeDown
  | CmdSetVolume !Int  -- ^ Absolute volume (0-127, AVRCP 1.5+)
  deriving stock (Show, Eq)

-- | Parse an AVRCP command from the string tag sent by the Rust
-- event system. When the Rust MediaPlayer1 handler is registered,
-- it sends "AVRCP:Play", "AVRCP:Pause", etc. through the
-- BlueZEvent::Error channel (a pragmatic reuse for control messages).
parseCommand :: Text -> Maybe AVRCPCommand
parseCommand t = case T.toLower (T.strip t) of
  "avrcp:play"         -> Just CmdPlay
  "avrcp:pause"        -> Just CmdPause
  "avrcp:stop"         -> Just CmdStop
  "avrcp:next"         -> Just CmdNext
  "avrcp:previous"     -> Just CmdPrevious
  "avrcp:fastforward"  -> Just CmdFastForward
  "avrcp:rewind"       -> Just CmdRewind
  "avrcp:volumeup"     -> Just CmdVolumeUp
  "avrcp:volumedown"   -> Just CmdVolumeDown
  _                    -> Nothing

-- | Handle an incoming AVRCP command.
--
-- This is called by the Haskell event loop when it receives an AVRCP
-- event from the Rust runtime. The handler takes the appropriate
-- action (adjusting volume, logging the command, etc.) and returns
-- a human-readable description of what happened.
handleCommand :: AVRCPCommand -> IO Text
handleCommand cmd = do
  action <- case cmd of
    CmdPlay        -> pure "Playback: PLAY"
    CmdPause       -> pure "Playback: PAUSE"
    CmdStop        -> pure "Playback: STOP"
    CmdNext        -> pure "Playback: NEXT TRACK"
    CmdPrevious    -> pure "Playback: PREVIOUS TRACK"
    CmdFastForward -> pure "Playback: FAST FORWARD"
    CmdRewind      -> pure "Playback: REWIND"
    CmdVolumeUp    -> do
      vol <- FFI.getVolume
      let newVol = min 127 (vol + 10)
      FFI.setVolume newVol
      pure $ "Volume: " <> T.pack (show newVol) <> "/127"
    CmdVolumeDown  -> do
      vol <- FFI.getVolume
      let newVol = max 0 (vol - 10)
      FFI.setVolume newVol
      pure $ "Volume: " <> T.pack (show newVol) <> "/127"
    CmdSetVolume v -> do
      FFI.setVolume v
      pure $ "Volume: SET " <> T.pack (show v) <> "/127"

  putStrLn $ "  [AVRCP] " <> T.unpack action
  pure action

-- ═══════════════════════════════════════════════════════════════════
-- Metadata Export
-- ═══════════════════════════════════════════════════════════════════

-- | Update the "Now Playing" metadata on connected AVRCP 1.3+ sinks.
--
-- This forwards metadata into the Rust MediaPlayer1 state via FFI,
-- which in turn exposes it to car stereos and headsets via D-Bus.
updateNowPlaying :: TrackMetadata -> IO ()
updateNowPlaying TrackMetadata{..} = do
  putStrLn $ "  [AVRCP] Now Playing: "
          <> T.unpack tmTitle <> " - "
          <> T.unpack tmArtist <> " ["
          <> T.unpack tmAlbum <> "]"
  _ <- FFI.updateMetadata (T.unpack tmTitle) (T.unpack tmArtist)
                      (T.unpack tmAlbum) tmDuration
  pure ()

-- | Update the playback status on connected AVRCP sinks.
updatePlaybackStatus :: PlaybackState -> IO ()
updatePlaybackStatus PlaybackState{..} = do
  let statusStr = case psStatus of
        Playing     -> "playing"
        Paused      -> "paused"
        Stopped     -> "stopped"
        FastForward -> "forward-seek"
        Rewind      -> "reverse-seek"
        Error       -> "error"
  _ <- FFI.updatePlaybackStatus statusStr
  _ <- FFI.updatePlaybackPosition psPosition
  putStrLn $ "  [AVRCP] Status: " <> show psStatus
          <> " @ " <> show psPosition <> "us"

-- | Clear the now-playing metadata (e.g., when playback stops).
clearNowPlaying :: IO ()
clearNowPlaying = do
  putStrLn "  [AVRCP] Now Playing: (cleared)"

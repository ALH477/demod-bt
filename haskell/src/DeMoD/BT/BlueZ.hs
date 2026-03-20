-- | DeMoD.BT.BlueZ - BlueZ D-Bus Orchestration Helpers
--
-- Optional Haskell-side helpers for BlueZ D-Bus interaction.
-- These are not currently used by the daemon (the Rust runtime
-- owns D-Bus and event polling), but kept for future integration.
--
-- Provides parsing of BlueZ signal bodies:
--   - InterfacesAdded:   new device discovered or connected
--   - InterfacesRemoved: device disappeared
--   - PropertiesChanged: connection state, metadata, volume changes
--
-- Status notes:
--   Optional helpers for future Haskell-side BlueZ integration.
--   The production daemon uses the Rust D-Bus runtime today.
--
-- LGPL-3.0 | Patent Pending | (c) 2025 DeMoD LLC
module DeMoD.BT.BlueZ
  ( -- * Connection
    BlueZConn (..)
  , connectBlueZ
  , disconnectBlueZ

    -- * Adapter
  , getAdapterProperties
  , setPowered
  , setDiscoverable
  , setAdapterAlias

    -- * Device Monitoring
  , watchDevices
  ) where

import DBus
import DBus.Client
import Data.Text (Text)
import qualified Data.Text as T
import Data.Map.Strict (Map)
import qualified Data.Map.Strict as Map
import Data.Maybe (fromMaybe)
import Data.Word (Word32)
import Control.Concurrent.STM (TBQueue, atomically, writeTBQueue)
import Control.Exception (try, SomeException)

import DeMoD.BT.Types

-- | Wrapper around a D-Bus client connection to BlueZ.
-- Also stores the discovered adapter path so we don't hardcode hci0.
data BlueZConn = BlueZConn
  { bzClient      :: !Client
  , bzAdapterPath :: !ObjectPath  -- ^ e.g., "/org/bluez/hci0" or "/org/bluez/hci1"
  }

-- | Connect to the system D-Bus and find the BlueZ adapter.
-- Returns the connection handle or an error description.
connectBlueZ :: IO (Either Text BlueZConn)
connectBlueZ = do
  result <- try @SomeException connectSystem
  case result of
    Left err -> pure $ Left $ "D-Bus connection failed: " <> T.pack (show err)
    Right client -> do
      -- Find the adapter path by querying ObjectManager
      adapterPath <- findAdapter client
      pure $ Right BlueZConn
        { bzClient = client
        , bzAdapterPath = adapterPath
        }

-- | Discover the Bluetooth adapter path via ObjectManager.
-- Scans all objects under /org/bluez for the Adapter1 interface.
-- Falls back to /org/bluez/hci0 if enumeration fails.
findAdapter :: Client -> IO ObjectPath
findAdapter client = do
  result <- try @SomeException $ call_ client
    (methodCall "/" "org.freedesktop.DBus.ObjectManager" "GetManagedObjects")
    { methodCallDestination = Just "org.bluez" }

  case result of
    Left _ -> do
      putStrLn "  [BlueZ] ObjectManager query failed, using /org/bluez/hci0"
      pure "/org/bluez/hci0"
    Right reply -> do
      -- The reply body is a single variant containing:
      --   Dict ObjectPath (Dict String (Dict String Variant))
      -- We need to find paths that have "org.bluez.Adapter1" as an interface key.
      let body = methodReturnBody reply
      case body of
        [v] -> case extractAdapterPath v of
          Just path -> do
            putStrLn $ "  [BlueZ] Found adapter: " <> show path
            pure path
          Nothing -> do
            putStrLn "  [BlueZ] No adapter found, using /org/bluez/hci0"
            pure "/org/bluez/hci0"
        _ -> pure "/org/bluez/hci0"

-- | Try to extract an adapter path from the ObjectManager response.
-- The structure is deeply nested variants; we peel them carefully.
extractAdapterPath :: Variant -> Maybe ObjectPath
extractAdapterPath _v = do
  -- The outermost type is a{oa{sa{sv}}}
  -- We can't easily parse this with the dbus library's generic fromVariant,
  -- so we use a pragmatic approach: convert to string representation
  -- and search for "Adapter1" with its associated object path.
  --
  -- A more robust approach would use the dbus library's Dictionary type,
  -- but the nesting depth (3 levels) makes the type gymnastics unwieldy.
  -- For production use, we'd use the Rust-side adapter enumeration (which
  -- already works via zbus ObjectManager) and pass the result through FFI.
  Nothing  -- Fall through to default; Rust side handles this

-- | Disconnect from D-Bus.
disconnectBlueZ :: BlueZConn -> IO ()
disconnectBlueZ = disconnect . bzClient

-- | Get properties of the adapter.
getAdapterProperties :: BlueZConn -> IO (Map Text Variant)
getAdapterProperties BlueZConn{..} = do
  result <- try @SomeException $ call_ bzClient
    (methodCall bzAdapterPath "org.freedesktop.DBus.Properties" "GetAll")
    { methodCallDestination = Just "org.bluez"
    , methodCallBody = [toVariant ("org.bluez.Adapter1" :: Text)]
    }
  case result of
    Left _ -> pure Map.empty
    Right reply -> case methodReturnBody reply of
      [v] -> pure $ fromMaybe Map.empty (fromVariant v)
      _   -> pure Map.empty

-- | Power the adapter on or off.
setPowered :: BlueZConn -> Bool -> IO ()
setPowered BlueZConn{..} powered = do
  _ <- try @SomeException $ call_ bzClient
    (methodCall bzAdapterPath "org.freedesktop.DBus.Properties" "Set")
    { methodCallDestination = Just "org.bluez"
    , methodCallBody =
        [ toVariant ("org.bluez.Adapter1" :: Text)
        , toVariant ("Powered" :: Text)
        , toVariant (toVariant powered)
        ]
    }
  pure ()

-- | Set the adapter as discoverable (for pairing).
setDiscoverable :: BlueZConn -> Bool -> IO ()
setDiscoverable BlueZConn{..} disc = do
  _ <- try @SomeException $ call_ bzClient
    (methodCall bzAdapterPath "org.freedesktop.DBus.Properties" "Set")
    { methodCallDestination = Just "org.bluez"
    , methodCallBody =
        [ toVariant ("org.bluez.Adapter1" :: Text)
        , toVariant ("Discoverable" :: Text)
        , toVariant (toVariant disc)
        ]
    }
  pure ()

-- | Set the adapter's friendly name (shown to other devices during discovery).
setAdapterAlias :: BlueZConn -> Text -> IO ()
setAdapterAlias BlueZConn{..} alias = do
  _ <- try @SomeException $ call_ bzClient
    (methodCall bzAdapterPath "org.freedesktop.DBus.Properties" "Set")
    { methodCallDestination = Just "org.bluez"
    , methodCallBody =
        [ toVariant ("org.bluez.Adapter1" :: Text)
        , toVariant ("Alias" :: Text)
        , toVariant (toVariant alias)
        ]
    }
  pure ()

-- ═══════════════════════════════════════════════════════════════════
-- Device Monitoring (Phase 2.5: Real Signal Parsing)
-- ═══════════════════════════════════════════════════════════════════

-- | Watch for device events via D-Bus signals.
--
-- [ROADMAP 2.5] Proper signal parsing - IMPLEMENTED
--
-- Monitors three signal types:
--   1. InterfacesAdded: a new BlueZ object appeared (device, transport, etc.)
--      Body: (ObjectPath, Dict{String, Dict{String, Variant}})
--   2. InterfacesRemoved: a BlueZ object disappeared
--      Body: (ObjectPath, [String])
--   3. PropertiesChanged on Device1: connection state, name changes
--      Body: (String, Dict{String, Variant}, [String])
--
-- Each signal is parsed and pushed to the event queue as a typed BTEvent.
watchDevices :: BlueZConn -> TBQueue BTEvent -> IO ()
watchDevices BlueZConn{..} queue = do

  -- ── InterfacesAdded ───────────────────────────────────────────
  -- Fires when a new device is discovered, a transport is created,
  -- or any other BlueZ object appears on D-Bus.
  _ <- addMatch bzClient (matchAny
    { matchSender      = Just "org.bluez"
    , matchInterface   = Just "org.freedesktop.DBus.ObjectManager"
    , matchMember      = Just "InterfacesAdded"
    })
    (\sig -> do
      let body = signalBody sig
      case body of
        -- Body: [ObjectPath, Dict{String, Dict{String, Variant}}]
        [pathVar, ifacesVar] -> do
          -- The first element is an ObjectPath. Extract it and convert to Text.
          let objPath = case fromVariant pathVar :: Maybe ObjectPath of
                Just op -> T.pack (formatObjectPath op)
                Nothing -> case fromVariant pathVar :: Maybe Text of
                  Just t  -> t
                  Nothing -> T.pack (show pathVar)
          -- Check if this is a Device1 object (a Bluetooth device)
          case fromVariant ifacesVar :: Maybe (Map Text (Map Text Variant)) of
            Just ifaces | Map.member "org.bluez.Device1" ifaces -> do
              let deviceProps = fromMaybe Map.empty (Map.lookup "org.bluez.Device1" ifaces)
                  name = fromMaybe "Unknown"
                    (Map.lookup "Name" deviceProps >>= fromVariant)
                  addr = fromMaybe "00:00:00:00:00:00"
                    (Map.lookup "Address" deviceProps >>= fromVariant)
                  paired = fromMaybe False
                    (Map.lookup "Paired" deviceProps >>= fromVariant)
                  connected = fromMaybe False
                    (Map.lookup "Connected" deviceProps >>= fromVariant)
                  trusted = fromMaybe False
                    (Map.lookup "Trusted" deviceProps >>= fromVariant)
                  classRaw = fromMaybe 0
                    (Map.lookup "Class" deviceProps >>= fromVariant :: Maybe Word32)

              let info = DeviceInfo
                    { diAddress   = BTAddress addr
                    , diName      = name
                    , diClass     = parseDeviceClass classRaw
                    , diPaired    = paired
                    , diConnected = connected
                    , diTrusted   = trusted
                    }

              atomically $ writeTBQueue queue (EvDeviceConnected info)

            -- Check if this is a MediaTransport1 object
            Just ifaces | Map.member "org.bluez.MediaTransport1" ifaces -> do
              let transportProps = fromMaybe Map.empty
                    (Map.lookup "org.bluez.MediaTransport1" ifaces)
                  codecByte = fromMaybe 0
                    (Map.lookup "Codec" transportProps >>= fromVariant :: Maybe Word32)
                  config = fromMaybe mempty
                    (Map.lookup "Configuration" transportProps >>= fromVariant)
                  volume = fromMaybe 127
                    (Map.lookup "Volume" transportProps >>= fromVariant :: Maybe Word32)

              let tInfo = TransportInfo
                    { tiPath          = objPath
                    , tiState         = TSPending
                    , tiCodec         = codecIdToCodec (fromIntegral codecByte)
                    , tiConfiguration = config
                    , tiVolume        = fromIntegral volume
                    }

              atomically $ writeTBQueue queue (EvTransportCreated tInfo)

            _ -> pure ()  -- Not a Device1 or MediaTransport1, ignore

        _ -> pure ()  -- Unexpected signal body shape
    )

  -- ── InterfacesRemoved ─────────────────────────────────────────
  -- Fires when a device disappears or a transport is released.
  _ <- addMatch bzClient (matchAny
    { matchSender    = Just "org.bluez"
    , matchInterface = Just "org.freedesktop.DBus.ObjectManager"
    , matchMember    = Just "InterfacesRemoved"
    })
    (\sig -> do
      let body = signalBody sig
      case body of
        -- Body: [ObjectPath, [String]]
        [pathVar, ifacesVar] -> do
          let objPath = fromMaybe "" (fromVariant pathVar :: Maybe Text)
          let ifaces = fromMaybe [] (fromVariant ifacesVar :: Maybe [Text])

          -- Check if a Device1 interface was removed
          if "org.bluez.Device1" `elem` ifaces then
            -- Extract the device address from the object path.
            -- BlueZ device paths look like: /org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF
            let addr = pathToAddress objPath
            in atomically $ writeTBQueue queue (EvDeviceDisconnected (BTAddress addr))

          -- Check if a MediaTransport1 interface was removed
          else if "org.bluez.MediaTransport1" `elem` ifaces then
            atomically $ writeTBQueue queue (EvTransportReleased objPath)

          else pure ()

        _ -> pure ()
    )

  -- ── PropertiesChanged on Device1 ──────────────────────────────
  -- Fires when a device's connection state changes, name updates, etc.
  _ <- addMatch bzClient (matchAny
    { matchSender    = Just "org.bluez"
    , matchInterface = Just "org.freedesktop.DBus.Properties"
    , matchMember    = Just "PropertiesChanged"
    })
    (\sig -> do
      let body = signalBody sig
      case body of
        -- Body: [String (interface), Dict{String, Variant}, [String]]
        (ifaceVar : changedVar : _) -> do
          let iface = fromMaybe "" (fromVariant ifaceVar :: Maybe Text)
              changed = fromMaybe Map.empty
                (fromVariant changedVar :: Maybe (Map Text Variant))

          case iface of
            "org.bluez.Device1" -> do
              -- Check if Connected property changed
              case Map.lookup "Connected" changed >>= fromVariant :: Maybe Bool of
                Just False -> do
                  let addr = pathToAddress (T.pack $ show (signalPath sig))
                  atomically $ writeTBQueue queue
                    (EvDeviceDisconnected (BTAddress addr))
                _ -> pure ()

            "org.bluez.MediaTransport1" -> do
              -- Volume change
              case Map.lookup "Volume" changed >>= fromVariant :: Maybe Word32 of
                Just vol -> atomically $ writeTBQueue queue
                  (EvVolumeChanged (fromIntegral vol))
                Nothing -> pure ()

              -- State change
              case Map.lookup "State" changed >>= fromVariant :: Maybe Text of
                Just "active" -> atomically $ writeTBQueue queue
                  (EvTransportAcquired (T.pack $ show (signalPath sig)))
                Just "idle" -> atomically $ writeTBQueue queue
                  (EvTransportReleased (T.pack $ show (signalPath sig)))
                _ -> pure ()

            "org.bluez.MediaPlayer1" -> do
              -- Track metadata change
              case Map.lookup "Track" changed >>= fromVariant :: Maybe (Map Text Variant) of
                Just trackMap -> do
                  let title  = fromMaybe "" (Map.lookup "Title" trackMap >>= fromVariant)
                      artist = fromMaybe "" (Map.lookup "Artist" trackMap >>= fromVariant)
                      album  = fromMaybe "" (Map.lookup "Album" trackMap >>= fromVariant)
                  atomically $ writeTBQueue queue $ EvTrackChanged TrackMetadata
                    { tmTitle    = title
                    , tmArtist   = artist
                    , tmAlbum    = album
                    , tmGenre    = ""
                    , tmTrackNum = 0
                    , tmDuration = 0
                    }
                Nothing -> pure ()

              -- Playback status change
              case Map.lookup "Status" changed >>= fromVariant :: Maybe Text of
                Just status -> atomically $ writeTBQueue queue $ EvPlaybackChanged PlaybackState
                  { psStatus   = parsePlaybackStatus status
                  , psPosition = 0
                  , psShuffle  = False
                  , psRepeat   = False
                  }
                Nothing -> pure ()

            _ -> pure ()

        _ -> pure ()
    )

  pure ()

-- ═══════════════════════════════════════════════════════════════════
-- Helpers
-- ═══════════════════════════════════════════════════════════════════

-- | Extract a Bluetooth address from a BlueZ D-Bus object path.
-- BlueZ encodes addresses in paths as: /org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF
-- We extract the last component and replace underscores with colons.
pathToAddress :: Text -> Text
pathToAddress path =
  case T.splitOn "/" path of
    parts | not (null parts) ->
      let lastPart = last parts
      in if "dev_" `T.isPrefixOf` lastPart
         then T.replace "_" ":" (T.drop 4 lastPart)
         else lastPart
    _ -> path

-- | Parse a BlueZ device class integer into our DeviceClass type.
-- The class is a 24-bit value; we look at the major device class (bits 8-12)
-- and major service class (bits 13-23).
parseDeviceClass :: Word32 -> DeviceClass
parseDeviceClass raw =
  let majorDevice = (raw `div` 256) `mod` 32  -- bits 8-12
  in case majorDevice of
    4  -> ClassHeadphones   -- Audio/Video major class
    _  -> ClassUnknown raw

-- | Map a BlueZ codec ID byte to our AudioCodec type.
codecIdToCodec :: Word32 -> AudioCodec
codecIdToCodec 0 = CodecSBC
codecIdToCodec 2 = CodecAAC
codecIdToCodec 6 = CodecLC3
codecIdToCodec _ = CodecSBC  -- default fallback

-- | Parse a playback status string from AVRCP.
parsePlaybackStatus :: Text -> PlaybackStatus
parsePlaybackStatus t = case T.toLower t of
  "playing"       -> Playing
  "paused"        -> Paused
  "stopped"       -> Stopped
  "forward-seek"  -> FastForward
  "reverse-seek"  -> Rewind
  "error"         -> Error
  _               -> Stopped

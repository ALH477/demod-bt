-- | DeMoD.BT - Top-Level Bluetooth Audio Library (Production)
--
-- [ROADMAP 2.2] AVDTP state machine integration - IMPLEMENTED
--
-- LGPL-3.0 | Patent Pending | (c) 2025 DeMoD LLC
module DeMoD.BT
  ( startDaemon
  , module DeMoD.BT.Types
  , module DeMoD.BT.AVDTP
  , module DeMoD.BT.AVRCP
  , module DeMoD.BT.BlueZ
  , module DeMoD.BT.DCF
  , module DeMoD.BT.FFI
  ) where

import Control.Concurrent (threadDelay)
import Control.Monad (forever, when)
import Data.Char (toLower)
import Data.IORef
import qualified Data.Text as T
import Text.Read (readMaybe)
import System.IO (hFlush, stdout)
import System.Environment (lookupEnv)

import DeMoD.BT.Types
import DeMoD.BT.AVDTP
import DeMoD.BT.AVRCP
import DeMoD.BT.BlueZ
import DeMoD.BT.DCF
import DeMoD.BT.FFI

startDaemon :: StreamDirection -> IO ()
startDaemon dirArg = do
  dir <- readDirection dirArg
  sampleRate <- readEnvInt "DEMOD_BT_SAMPLE_RATE" 44100
  channels <- readEnvInt "DEMOD_BT_CHANNELS" 2
  jitterMs <- readEnvInt "DEMOD_BT_JITTER_MS" 40
  dcfPayload <- readEnvInt "DEMOD_BT_DCF_PAYLOAD" (fromIntegral dcfOptimalPayload)

  putStrLn $ "  Version:     " <> getVersion
  putStrLn $ "  Direction:   " <> show dir
  putStrLn $ "  DCF:         " <> show dcfHeaderSize <> "B header + "
                               <> show dcfOptimalPayload <> "B payload = 256B"
  putStrLn ""

  result <- initPipeline (fromIntegral sampleRate) (fromIntegral channels)
                        dir (fromIntegral jitterMs) (fromIntegral dcfPayload)
  case result of
    Left err -> putStrLn $ "  FATAL: " <> err
    Right () -> do
      putStrLn "  [ok] Rust pipeline initialized"
      regResult <- registerEndpoints
      case regResult of
        Left err -> do
          putStrLn $ "  FATAL: " <> err
          shutdownPipeline
        Right () -> do
          putStrLn "  [ok] A2DP endpoint registered"
          putStrLn "  [ok] Waiting for connections...\n"
          sessionRef <- newIORef (SomeSession IdleSession)
          counterRef <- newIORef (0 :: Int)
          eventLoop sessionRef counterRef
          shutdownPipeline
  putStrLn "\n  Shut down."

readEnvInt :: String -> Int -> IO Int
readEnvInt key def = do
  v <- lookupEnv key
  pure $ case v >>= readMaybe of
    Just n  -> n
    Nothing -> def

readDirection :: StreamDirection -> IO StreamDirection
readDirection def = do
  v <- lookupEnv "DEMOD_BT_DIRECTION"
  pure $ case fmap (map toLower) v of
    Just "sink"   -> Sink
    Just "source" -> Source
    _             -> def

eventLoop :: IORef SomeSession -> IORef Int -> IO ()
eventLoop sessionRef counterRef = forever $ do
  mEvent <- pollEvent
  case mEvent of
    Nothing -> pure ()
    Just evt -> handleEvent sessionRef evt

  counter <- readIORef counterRef
  writeIORef counterRef (counter + 1)
  when (counter `mod` 100 == 0) $ do
    printMetrics
    ss <- readIORef sessionRef
    putStrLn $ "  [S] " <> show ss
  threadDelay 50_000

handleEvent :: IORef SomeSession -> FfiEvent -> IO ()
handleEvent sessionRef FfiEvent{..} = case feEventType of

  EvtDeviceConnected ->
    case feStringData of
      Nothing -> putStrLn "  [+] Device connected"
      Just s  -> case break (=='|') s of
        (addr, '|':name) -> putStrLn $ "  [+] Device: " <> addr <> " (" <> name <> ")"
        _ -> putStrLn $ "  [+] Device: " <> s

  EvtDeviceDisconnected -> do
    putStrLn $ "  [-] Disconnected: " <> maybe "?" id feStringData
    streaming <- isStreaming
    when streaming $ do
      putStrLn "  [x] Stopping stream"
      stopStream
    modifyIORef' sessionRef $ \ss -> case driveEventPure ss EvAbort of
      Just io -> io
      Nothing -> ss

  EvtTransportPending -> do
    let path = maybe "" id feStringData
    streaming <- isStreaming
    if streaming
      then pure ()
      else do
        putStrLn $ "  [>] Transport pending: " <> path
        result <- acquireAndStart path
        case result of
          Right () -> pure ()
          Left err -> putStrLn $ "  [!] Acquire failed: " <> err

  EvtTransportAcquired -> do
    let path = maybe "" id feStringData
    putStrLn $ "  [>] Transport acquired: " <> path
    vol <- getVolume
    putStrLn $ "  [>] STREAMING (vol " <> show vol <> "/127)"
    -- Drive AVDTP: Configured -> Open -> Streaming
    ss <- readIORef sessionRef
    ss1 <- driveEvent ss (EvTransportOpened (T.pack path))
    ss2 <- driveEvent ss1 EvStreamStarted
    writeIORef sessionRef ss2

  EvtTransportReleased -> do
    putStrLn $ "  [x] Released: " <> maybe "" id feStringData
    stopStream
    ss <- readIORef sessionRef
    ss' <- driveEvent ss EvTransportClosed
    writeIORef sessionRef ss'

  EvtCodecNegotiated -> do
    let codecStr = maybe "?" id feStringData
        codec = case map toLower codecStr of
          "sbc"  -> CodecSBC
          "aac"  -> CodecAAC
          "lc3"  -> CodecLC3
          _      -> CodecSBC
    putStrLn $ "  [C] Codec: " <> codecStr
    ss <- readIORef sessionRef
    ss' <- driveEvent ss (EvCodecConfigured codec mempty (T.pack codecStr))
    writeIORef sessionRef ss'

  EvtVolumeChanged -> do
    case feStringData >>= readMaybe of
      Nothing -> pure ()
      Just vol -> do
        setVolumeRemote vol
        putStrLn $ "  [V] Volume: " <> show vol <> "/127"

  EvtError -> do
    let msg = maybe "" id feStringData
    case parseCommand (T.pack msg) of
      Just cmd -> handleCommand cmd >> pure ()
      Nothing  -> putStrLn $ "  [!] " <> msg

  EvtNone -> pure ()

-- | Helper: try to drive an event without IO (for modifyIORef').
-- Returns Nothing if IO is needed (use driveEvent directly instead).
driveEventPure :: SomeSession -> AVDTPEvent -> Maybe SomeSession
driveEventPure _ EvAbort = Just (SomeSession IdleSession)
driveEventPure _ _       = Nothing

printMetrics :: IO ()
printMetrics = do
  streaming <- isStreaming
  when streaming $ do
    mMetrics <- getMetrics
    case mMetrics of
      Nothing -> pure ()
      Just MetricsSnapshot{..} -> do
        putStr $ "\r  [~] f=" <> show msFramesProcessed
              <> " u=" <> show msUnderruns
              <> " o=" <> show msOverruns
              <> " b=" <> show msBufferLevel <> "    "
        hFlush stdout

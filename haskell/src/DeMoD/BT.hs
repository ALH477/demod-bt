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
import Data.IORef
import qualified Data.Text as T
import System.IO (hFlush, stdout)

import DeMoD.BT.Types
import DeMoD.BT.AVDTP
import DeMoD.BT.AVRCP
import DeMoD.BT.BlueZ
import DeMoD.BT.DCF
import DeMoD.BT.FFI

startDaemon :: StreamDirection -> IO ()
startDaemon dir = do
  putStrLn $ "  Version:     " <> getVersion
  putStrLn $ "  Direction:   " <> show dir
  putStrLn $ "  DCF:         " <> show dcfHeaderSize <> "B header + "
                               <> show dcfOptimalPayload <> "B payload = 256B"
  putStrLn ""

  result <- initPipeline 44100 2 dir 40 dcfOptimalPayload
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
    putStrLn $ "  [+] Device: " <> maybe "?" id feStringData

  EvtDeviceDisconnected -> do
    putStrLn $ "  [-] Disconnected: " <> maybe "?" id feStringData
    streaming <- isStreaming
    when streaming $ do
      putStrLn "  [x] Stopping stream"
      stopStream
    modifyIORef' sessionRef $ \ss -> case driveEventPure ss EvAbort of
      Just io -> io
      Nothing -> ss

  EvtTransportAcquired -> do
    let path = maybe "" id feStringData
    putStrLn $ "  [>] Transport: " <> path
    result <- acquireAndStart path
    case result of
      Right () -> do
        vol <- getVolume
        putStrLn $ "  [>] STREAMING (vol " <> show vol <> "/127)"
        -- Drive AVDTP: Idle -> Configured -> Open -> Streaming
        ss <- readIORef sessionRef
        ss1 <- driveEvent ss (EvCodecConfigured CodecSBC mempty (T.pack path))
        ss2 <- driveEvent ss1 (EvTransportOpened (T.pack path))
        ss3 <- driveEvent ss2 EvStreamStarted
        writeIORef sessionRef ss3
      Left err -> putStrLn $ "  [!] Start failed: " <> err

  EvtTransportReleased -> do
    putStrLn $ "  [x] Released: " <> maybe "" id feStringData
    stopStream
    ss <- readIORef sessionRef
    ss' <- driveEvent ss EvTransportClosed
    writeIORef sessionRef ss'

  EvtCodecNegotiated -> do
    let codec = maybe "?" id feStringData
    putStrLn $ "  [C] Codec: " <> codec
    ss <- readIORef sessionRef
    ss' <- driveEvent ss (EvCodecConfigured CodecSBC mempty (T.pack codec))
    writeIORef sessionRef ss'

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

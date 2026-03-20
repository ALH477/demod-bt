-- | DeMoD.BT.AVDTP - Type-Safe AVDTP State Machine (Production)
--
-- AVDTP state machine encoded at the type level using GADTs and DataKinds.
-- Illegal transitions are compile errors. A 'SomeSession' existential
-- wrapper enables runtime storage in an IORef while preserving type safety
-- at each individual transition site.
--
-- [ROADMAP 2.2] AVDTP state machine integration - IMPLEMENTED
--
-- LGPL-3.0 | Patent Pending | (c) 2025 DeMoD LLC
module DeMoD.BT.AVDTP
  ( -- * State Types
    AVDTPState (..)
    -- * Session GADT
  , Session (..)
    -- * Existential Wrapper (for IORef storage)
  , SomeSession (..)
  , someSessionState
    -- * State Transitions
  , configure
  , open
  , start
  , suspend
  , close
  , abort
    -- * Event-Driven Transitions
  , driveEvent
    -- * Queries
  , sessionCodec
  , sessionTransport
  ) where

import Data.ByteString (ByteString)
import Data.Text (Text)
import DeMoD.BT.Types (AudioCodec, TransportPath)

-- ═══════════════════════════════════════════════════════════════════
-- State Kind
-- ═══════════════════════════════════════════════════════════════════

data AVDTPState
  = Idle
  | Configured
  | Open
  | Streaming
  | Closing
  deriving stock (Show, Eq, Ord)

-- ═══════════════════════════════════════════════════════════════════
-- Session GADT
-- ═══════════════════════════════════════════════════════════════════

data Session (s :: AVDTPState) where
  IdleSession :: Session 'Idle

  ConfiguredSession
    :: { csCodec    :: !AudioCodec
       , csConfig   :: !ByteString
       , csEndpoint :: !Text
       }
    -> Session 'Configured

  OpenSession
    :: { osCodec     :: !AudioCodec
       , osConfig    :: !ByteString
       , osEndpoint  :: !Text
       , osTransport :: !TransportPath
       }
    -> Session 'Open

  StreamingSession
    :: { ssCodec     :: !AudioCodec
       , ssConfig    :: !ByteString
       , ssEndpoint  :: !Text
       , ssTransport :: !TransportPath
       }
    -> Session 'Streaming

  ClosingSession
    :: { clTransport :: !TransportPath }
    -> Session 'Closing

-- ═══════════════════════════════════════════════════════════════════
-- Existential Wrapper
-- ═══════════════════════════════════════════════════════════════════

-- | Existential wrapper that hides the state parameter, allowing
-- a Session to be stored in an IORef. Each transition unwraps,
-- verifies the state at runtime, performs the type-safe transition,
-- and re-wraps.
data SomeSession = forall s. SomeSession (Session s)

-- | Get the current state name from an existential session.
someSessionState :: SomeSession -> AVDTPState
someSessionState (SomeSession s) = case s of
  IdleSession{}       -> Idle
  ConfiguredSession{} -> Configured
  OpenSession{}       -> Open
  StreamingSession{}  -> Streaming
  ClosingSession{}    -> Closing

instance Show SomeSession where
  show ss = "SomeSession(" <> show (someSessionState ss) <> ")"

-- ═══════════════════════════════════════════════════════════════════
-- Type-Safe Transitions
-- ═══════════════════════════════════════════════════════════════════

configure
  :: Session 'Idle -> AudioCodec -> ByteString -> Text
  -> IO (Session 'Configured)
configure IdleSession codec config endpoint = do
  putStrLn $ "  [AVDTP] Idle -> Configured: " <> show codec
  pure ConfiguredSession
    { csCodec = codec, csConfig = config, csEndpoint = endpoint }

open :: Session 'Configured -> TransportPath -> IO (Session 'Open)
open ConfiguredSession{..} transport = do
  putStrLn $ "  [AVDTP] Configured -> Open: " <> show transport
  pure OpenSession
    { osCodec = csCodec, osConfig = csConfig
    , osEndpoint = csEndpoint, osTransport = transport }

start :: Session 'Open -> IO (Session 'Streaming)
start OpenSession{..} = do
  putStrLn "  [AVDTP] Open -> Streaming"
  pure StreamingSession
    { ssCodec = osCodec, ssConfig = osConfig
    , ssEndpoint = osEndpoint, ssTransport = osTransport }

suspend :: Session 'Streaming -> IO (Session 'Open)
suspend StreamingSession{..} = do
  putStrLn "  [AVDTP] Streaming -> Open (suspended)"
  pure OpenSession
    { osCodec = ssCodec, osConfig = ssConfig
    , osEndpoint = ssEndpoint, osTransport = ssTransport }

close :: Session 'Open -> IO (Session 'Closing)
close OpenSession{..} = do
  putStrLn $ "  [AVDTP] Open -> Closing: " <> show osTransport
  pure ClosingSession { clTransport = osTransport }

abort :: Session s -> IO (Session 'Idle)
abort _ = do
  putStrLn "  [AVDTP] -> Idle (abort)"
  pure IdleSession

-- ═══════════════════════════════════════════════════════════════════
-- Event-Driven Transitions
-- ═══════════════════════════════════════════════════════════════════

-- | Events that drive the AVDTP state machine. These map directly
-- to the FFI events from the Rust runtime.
data AVDTPEvent
  = EvCodecConfigured !AudioCodec !ByteString !Text
  | EvTransportOpened !TransportPath
  | EvStreamStarted
  | EvStreamSuspended
  | EvTransportClosed
  | EvAbort
  deriving stock (Show)

-- | Drive the state machine forward based on a runtime event.
--
-- This is the bridge between the existential SomeSession (stored
-- in an IORef) and the type-safe transitions. Each case pattern-
-- matches on both the current state AND the event, calling the
-- appropriate typed transition. Invalid combinations (e.g.,
-- StreamStarted while Idle) are logged and ignored.
--
-- [ROADMAP 2.2] Event-driven AVDTP integration - IMPLEMENTED
driveEvent :: SomeSession -> AVDTPEvent -> IO SomeSession
driveEvent (SomeSession session) event = case (session, event) of

  -- Idle + CodecConfigured -> Configured
  (IdleSession, EvCodecConfigured codec config ep) -> do
    s <- configure IdleSession codec config ep
    pure (SomeSession s)

  -- Configured + TransportOpened -> Open
  (s@ConfiguredSession{}, EvTransportOpened path) -> do
    s' <- open s path
    pure (SomeSession s')

  -- Open + StreamStarted -> Streaming
  (s@OpenSession{}, EvStreamStarted) -> do
    s' <- start s
    pure (SomeSession s')

  -- Streaming + StreamSuspended -> Open
  (s@StreamingSession{}, EvStreamSuspended) -> do
    s' <- suspend s
    pure (SomeSession s')

  -- Open + TransportClosed -> Closing -> Idle
  (s@OpenSession{}, EvTransportClosed) -> do
    _ <- close s
    s' <- abort ClosingSession { clTransport = osTransport s }
    pure (SomeSession s')

  -- Streaming + TransportClosed -> abort to Idle
  (s@StreamingSession{}, EvTransportClosed) -> do
    s' <- abort s
    pure (SomeSession s')

  -- Any state + Abort -> Idle
  (s, EvAbort) -> do
    s' <- abort s
    pure (SomeSession s')

  -- Invalid transition: log and stay in current state
  _ -> do
    putStrLn $ "  [AVDTP] Ignoring event " <> show event
            <> " in state " <> show (someSessionState (SomeSession session))
    pure (SomeSession session)

-- ═══════════════════════════════════════════════════════════════════
-- Queries
-- ═══════════════════════════════════════════════════════════════════

sessionCodec :: Session s -> Maybe AudioCodec
sessionCodec IdleSession           = Nothing
sessionCodec ConfiguredSession{..} = Just csCodec
sessionCodec OpenSession{..}       = Just osCodec
sessionCodec StreamingSession{..}  = Just ssCodec
sessionCodec ClosingSession{}      = Nothing

sessionTransport :: Session s -> Maybe TransportPath
sessionTransport OpenSession{..}      = Just osTransport
sessionTransport StreamingSession{..} = Just ssTransport
sessionTransport ClosingSession{..}   = Just clTransport
sessionTransport _                    = Nothing

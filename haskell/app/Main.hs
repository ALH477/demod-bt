-- | Main - DeMoD BT Daemon Entry Point
--
-- Parses command-line arguments and delegates to the library's
-- startDaemon function, which handles the full lifecycle.
--
-- LGPL-3.0 | Patent Pending | (c) 2025 DeMoD LLC
module Main (main) where

import Control.Exception (bracket_)
import System.IO (hFlush, stdout, hSetBuffering, BufferMode(..))
import DeMoD.BT (startDaemon, shutdownPipeline)
import DeMoD.BT.Types (StreamDirection(..))

banner :: String
banner = unlines
  [ ""
  , "  ____        __  __       ____     ____ _____"
  , " |  _ \\  ___ |  \\/  | ___ |  _ \\   | __ )_   _|"
  , " | | | |/ _ \\| |\\/| |/ _ \\| | | |  |  _ \\ | |"
  , " | |_| |  __/| |  | | (_) | |_| |  | |_) || |"
  , " |____/ \\___||_|  |_|\\___/|____/   |____/ |_|"
  , ""
  , "  Bluetooth Audio Sink/Source"
  , "  LGPL-3.0 | Patent Pending | USAF Validated"
  , "  Created by Asher - DeMoD LLC"
  , ""
  ]

main :: IO ()
main = do
  hSetBuffering stdout LineBuffering
  putStr banner
  hFlush stdout

  -- Default to Sink mode. A full implementation would parse CLI args
  -- via optparse-applicative, but this gets audio flowing immediately.
  let direction = Sink

  putStrLn "  Press Ctrl+C to stop.\n"
  startDaemon direction

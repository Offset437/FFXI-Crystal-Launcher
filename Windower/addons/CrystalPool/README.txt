add "lua load CrystalPool" to the windower/scripts/init.txt before any other addons are loaded.

remove any lines which load XIPivot Crystal Pool will load this itself once the asset pool has been increased.

After character login, look for:
   [CrystalPool] 1024 MiB verified (Crystal Launcher); XIPivot loading requested.

   The host log should also contain:
   asset pool: 1024 MiB verified

   Use //crystalpool to check again or retry verification.

CrystalPool uses the supplied _CrystalPool.dll to read the verification signal
published by this game host only after measuring the live asset pool.
It waits up to two minutes for verification and requests XIPivot only after
success. A timeout can be retried with //crystalpool.
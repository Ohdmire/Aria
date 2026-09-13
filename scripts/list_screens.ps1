Add-Type -AssemblyName System.Windows.Forms
[System.Windows.Forms.Screen]::AllScreens | ForEach-Object {
  '{0} {1} primary={2}' -f $_.DeviceName, $_.Bounds, $_.Primary
}

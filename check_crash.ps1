Get-WinEvent -FilterHashtable @{LogName='Application'; StartTime=(Get-Date).AddHours(-2)} -MaxEvents 100 |
  Where-Object { $_.Message -match 'shareflow' -or $_.ProviderName -eq 'Application Error' } |
  Select-Object -First 10 |
  Format-List TimeCreated,Id,LevelDisplayName,ProviderName,Message

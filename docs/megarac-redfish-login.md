# MegaRAC / Gigabyte BMC: Redfish rejects a user that the web UI accepts

Applies to AMI MegaRAC SP-X firmware as shipped on Gigabyte servers (seen on an R181-NA0
with BMC firmware 12.60 and 12.61). Other AMI-based BMCs may behave the same way.

## Symptom

- The console tab connects and shows video, but the **Health & SEL** tab (and the alarm
  checker and the log export) report *invalid username or password* for the same account,
  with a **Change Password…** button.
- Outside the app, Redfish answers `401` with `Base.1.5.AccessDenied` (older firmware:
  `Security.1.0.AccessDenied`) to everything except the service root, `$metadata` and
  `odata`, while the same credentials log in to the web UI:

  ```sh
  curl -k -u oob-admin:'…' https://10.2.31.69/redfish/v1/Systems
  # {"error":{"code":"Base.1.5.AccessDenied", … "the service was denied access."}}
  ```

- Only the factory `admin` account works over Redfish.
- The BMC's Redfish audit log (`/redfish/v1/Managers/Self/LogServices/AuditLog/Entries`)
  records the attempts as `User: in-band-user, … the service was denied access`: the
  credentials were not matched to any account at all.

## Cause

The Redfish service keeps its own account store. With `PAMEnabled` off in
`/redfish/v1/AccountService/Configurations` it checks only that store, and only the default
`admin` has a password there. Users created in the web UI (IPMI users) appear in
`/redfish/v1/AccountService/Accounts` as "User Account sync from IPMI" with an empty
password (or do not appear at all), so Redfish can never authenticate them.

Things that do **not** help: the web UI's user editor (it has no Redfish option), the
user list's "sync" button, enabling the synced account (`PATCH … {"Enabled": true}`),
setting its password over Redfish, resetting the password in the web UI, or renaming the
user (a `-` in the name is fine).

## Fix

Turn PAM on, so Redfish authenticates against the same user database as the web UI. This
is done over Redfish as `admin` (or any account that already works there); it takes effect
immediately, needs no reboot, and is reversible with `false`.

```sh
BMC=https://10.2.31.69
# 1. The ETag of the configuration (the BMC refuses a PATCH without If-Match: HTTP 428).
ETAG=$(curl -sk -D - -o /dev/null -u admin:'…' "$BMC/redfish/v1/AccountService/Configurations" \
       | grep -i '^ETag' | tr -d '\r' | cut -d' ' -f2-)
# 2. Enable PAM.
curl -k -u admin:'…' -H "If-Match: $ETAG" -H 'Content-Type: application/json' \
     -X PATCH -d '{"PAMEnabled": true}' "$BMC/redfish/v1/AccountService/Configurations"
# 3. Check: every web-UI user can now use Redfish.
curl -k -u oob-admin:'…' "$BMC/redfish/v1/Systems/Self"
```

The configuration should then read:

```json
{ "PAMEnabled": true, "PAMOrder": ["IPMI", "LDAP", "ACTIVE DIRECTORY", "RADIUS"] }
```

## Notes

- With PAM on, every HTTP basic-auth request by a PAM (web UI) user is a login, and the BMC
  refuses the fifth within about a minute (`401`, audit log again says `in-band-user`). The
  factory `admin` has no such limit. Rust Drac therefore opens one Redfish session per BMC
  and user and sends its token instead; scripts should do the same
  (`POST /redfish/v1/SessionService/Sessions`, then `X-Auth-Token`).
- HTTP `428 Ami.1.0.PreconditionHeaderMissing` means the `If-Match` header was missing or
  stale; fetch the ETag again.
- Five failed logins lock the account for 30 seconds (`AccountLockoutThreshold` /
  `AccountLockoutDuration` in `/redfish/v1/AccountService`), so wait before retrying.
- The `admin` account is disabled on some installations ("Enable User Access" in the web
  UI's user editor), usually with a password nobody remembers. Enable it there **and set a
  new password** in the same save: the editor writes the password into the Redfish store
  for the factory account, so `admin` works over Redfish right away. Run the PATCH above as
  `admin`, then disable the account again. The BMC refuses to re-set the password an
  account already has (`Error in modifying user password, attempt to set previous
  password`, code 19000), so re-saving a user with its current password does not re-sync it.
- On one BMC (its `admin` had been re-created later than on its siblings) the password set
  in the editor never reached the Redfish store: IPMI accepted the new password, Redfish
  kept answering `401`, and no account could log in over Redfish at all. A BMC cold reset
  changed nothing. What fixed it was rebuilding the Redfish store from the web UI:
  **Maintenance → Preserve Configuration**, tick everything *except* Redfish, then
  **Maintenance → Restore Factory Defaults** (ticked items are the ones *kept*; the page
  shows the preserve list). The BMC restarts (about two minutes), re-syncs the accounts
  from IPMI with their current passwords, and every web-UI user then works over Redfish
  even with PAM off. Over the API that is `PUT /api/maintenance/preserve` and
  `PUT /api/maintenance/restore_defaults`, both with
  `{"id":1,"sdr":1,"fru":1,"sel":1,"ipmi":1,"network":1,"ntp":1,"snmp":1,"ssh":1,"kvm":1,"authentication":1,"syslog":1,"web":1,"redfish":0}`.
  Where some Redfish account still works,
  `/redfish/v1/Managers/Self/Actions/Oem/AMIManager.RedfishDBReset`
  (`{"RedfishDBResetType": "ResetAll"}`) does the same without the web UI. Both discard
  every Redfish-side setting (subscriptions, certificates, the audit log). Try PAM first.

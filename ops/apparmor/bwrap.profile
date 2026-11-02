abi <abi/4.0>,
include <tunables/global>

# An operator may install this on hosts whose AppArmor policy blocks bwrap's
# user namespaces. It leaves the global restriction in place. The connector
# installer does not change host security policy automatically.
profile bwrap /usr/bin/bwrap flags=(unconfined) {
  userns,
}

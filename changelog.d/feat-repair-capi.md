### Added

- **The repair through the C ABI (#477).** `fs_xfs_repair_with_fs_core_device`
  plans a repair of the volume on an `fs_core` device, with every rule
  `fsck.xfs -y` has, and with `apply` set writes it. Each change (its
  offset, length, finding code and rule), each refusal and each unplanned
  finding is passed to a callback, which can stop the listing and cancel
  the repair before anything is written. A report says whether a plan was
  made, how many changes it proposed and wrote, and whether the volume
  checks clean afterwards. A host that reaches its device through callbacks
  or a partition slice can now repair a volume as well as check it.

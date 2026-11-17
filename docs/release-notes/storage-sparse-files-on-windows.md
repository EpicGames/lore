---
type: feature
---

- `lore-capi`: On Windows, on NTFS and ReFS, `lore_storage_put_file` and
  `lore_storage_put_file_resolved` with `skip_holes` set store the holes of a file as they do on
  Linux. A sparse file's allocated ranges are data and the ranges it leaves unallocated are holes.
  On NTFS, everything past the file's valid data length, where what was ever written to it ends,
  is a hole too, whether the file is sparse or not, so a file extended or preallocated past what
  was written stores that tail as a hole; on ReFS that tail is stored as data, as before. On ReFS a
  put of a sparse file cut short before its data was written out and grown again, or of one written
  through a memory mapping still open while it is put, can store ranges no write reached as zeros,
  as data. Written
  zeros are data, except where the file system itself leaves them unallocated, as NTFS can for a
  small write of zeros alone in an allocation unit of a sparse file. A put that cannot write the
  file's data out first, as for a read-only file or while another process holds the file open
  without sharing write access, stores the whole file as data, with no hole, and so does a put of
  a file whose data the file system may keep elsewhere until it is read, such as a cloud
  placeholder or an offline file. A put without `skip_holes` reads the file whole, as before.
- `lore-capi`: On Windows, `lore_storage_get_file` and `lore_storage_get_file_resolved` mark the
  file they write for content larger than one fragment sparse before sizing it, so holes in the
  content come back as holes there, for a fresh get and for one that resumes from its temporary
  file, and they write zeros stored as data as data. A get over an earlier file releases that
  file's storage whole first, so none of its ranges stays allocated in a hole of the content.

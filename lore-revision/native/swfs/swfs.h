#ifndef SWFS_H_
#define SWFS_H_

#define SWFSDate    "2026-07-20"
#define SWFSCopyright "Copyright (C) 1994-2026, Epic Game Tools, Inc."

#define SWFSVersion     "2026.07"
#define SWFSMajorVersion    2026
#define SWFSMinorVersion       7
#define SWFSReleaseNumber      3

#define SWFSCustomization      0
#define SWFSBuildNumber        0


//API for clients using the library

typedef unsigned char		swfs_ubyte;
typedef char				swfs_utf8;
typedef wchar_t				swfs_utf16;
typedef unsigned char		swfs_u8;
typedef unsigned short		swfs_u16;
typedef unsigned int		swfs_u32;
typedef unsigned long long	swfs_u64;
typedef void *				SWFSHandle;

#ifdef _MSC_VER
#define SWFS_API_FN				__declspec(dllexport)
#else
#define SWFS_API_FN				__attribute__((visibility("default")))
#endif

#ifdef __cplusplus
#define SWFS_API_START			extern "C" {
#define SWFS_API_END			}
#else //__cplusplus
#define SWFS_API_START
#define SWFS_API_END
#endif //__cplusplus

#define SWFS_MAX_PATH				32768
//the longest name of a file or directory: what Windows allows in one path component, 255 UTF-16 units.  Names are UTF-8 in
//this API, and a unit takes at most 3 bytes of it (a character outside the BMP is 2 units and 4 bytes), so a name is at most
//SWFS_MAX_FILENAME_LENGTH bytes; it has to satisfy both
#define SWFS_MAX_FILENAME_UTF16		255
#define SWFS_MAX_FILENAME_LENGTH	(SWFS_MAX_FILENAME_UTF16 * 3)

SWFS_API_START

#define SWFS_BUFF_ALIGN_LG2 16				//all direct file access buffers must be aligned to 64kb
#define SWFS_BUFF_ALIGN_BYTES(num_bytes)	((((size_t)(num_bytes)) + (1ULL << SWFS_BUFF_ALIGN_LG2) - 1ULL) & ~((1ULL << SWFS_BUFF_ALIGN_LG2) - 1ULL))
#define SWFS_READ_WHOLE_FILE				(~(size_t)0 & ~(size_t)((1ULL << SWFS_BUFF_ALIGN_LG2) - 1ULL))	//the largest aligned num_bytes: swfsReadFrozenFile() from 0 with it reads everything

//Error codes under SWFSResult_FirstError are HRESULTS
typedef enum {
	SWFSResult_Success,

	SWFSResult_FirstError		= 0x80000000,

	SWFSResult_FileNotFound,
	SWFSResult_BufferNotAligned,				//IO buffers must be aligned to 2^SWFS_BUFF_ALIGN_LG2 bytes (64kb)
	SWFSResult_BufferTooSmall,					//IO buffers must hold what the callback's first call asked for: the range read, rounded up to whole blocks
	SWFSResult_VirtualFileReadFailed,			//the read_file callback returned fewer bytes than the file's size says exist
	SWFSResult_ThawPathConflict,				//a swfsThaw() entry, or the source's listing at the end of the thaw, would turn an existing file into a directory or a
												//directory into a file ("x\y" with x a file, say), and what is there has something of the user's (a change the client
												//has not seen, or a file or directory the user made or moved): the entry is skipped and the local node kept.  One that
												//is the source's own, unchanged here, is replaced and nothing is reported.  Also a list that names one thing as both a
												//file and a directory: the first entry wins
	SWFSResult_RangeNotAligned,					//swfsReadFrozenFile()'s offset and num_bytes must be multiples of 2^SWFS_BUFF_ALIGN_LG2 bytes (64kb)
	SWFSResult_DataCorruption,					//a data block's bytes do not match the hash recorded for them (SWFSInitFlag_BlockHashes).  Fatal, unless
												//SWFSInitFlag_TryBackupBlockOnDataCorruption is set: then it is reported through the callback and the read goes on
} SWFSResult_Enum;


#define NODE_MOD_FLAG_XM_LIST																	\
										NODE_MOD_FLAG_XM(NewNode					, 'N', 0)	\
										NODE_MOD_FLAG_XM(ChangeFlags				, 'F', 1)	\
										NODE_MOD_FLAG_XM(ChangeTree					, 'T', 2)	\
										NODE_MOD_FLAG_XM(Deleted					, 'D', 3)	\
										NODE_MOD_FLAG_XM(UpdateTimeCreate			, 'T', 4)	\
										NODE_MOD_FLAG_XM(UpdateTimeRead				, 'R', 5)	\
										NODE_MOD_FLAG_XM(UpdateTimeWrite			, 'W', 6)	\
										NODE_MOD_FLAG_XM(UpdateTimeChange			, 'G', 7)	\
										NODE_MOD_FLAG_XM(UpdateSize					, 'Z', 8)	\
										NODE_MOD_FLAG_XM(UpdateAttrs				, 'A', 9)	\
										NODE_MOD_FLAG_XM(UpdateName					, 'N', 10)	\
										NODE_MOD_FLAG_XM(DeVirtualize				, 'I', 11)	\
										NODE_MOD_FLAG_XM(Hydrate					, 'H', 12)	\
										NODE_MOD_FLAG_XM(BlockReserve				, 'V', 13)	\
										NODE_MOD_FLAG_XM(BlockAlloc					, 'M', 14)	\
										NODE_MOD_FLAG_XM(ChangedWhileFrozen			, 'F', 15)	\


#define NODE_MOD_FLAG_XM(name, flag_char, bit,  ...)	static const swfs_u32 SWFSNodeModFlag_##name = 1u << (bit);
NODE_MOD_FLAG_XM_LIST
#undef NODE_MOD_FLAG_XM

static const swfs_u32 SWFSNodeModFlag_DontFreeze = SWFSNodeModFlag_ChangedWhileFrozen;	//the same bit, reused for the client's answer in swfsFreeze()'s callback: the list never reports ChangedWhileFrozen itself

#define SWFS_LOG_FLAG_XM_LIST							\
			SWFS_LOG_FLAG_XM(EventWrite,    0)			\
			SWFS_LOG_FLAG_XM(EventRead,     1)			\
			SWFS_LOG_FLAG_XM(ModifyNode,    2)			\
			SWFS_LOG_FLAG_XM(AllocNode,     3)			\
			SWFS_LOG_FLAG_XM(AllocMetadata, 4)			\
			SWFS_LOG_FLAG_XM(Freeze,        5)			\
			SWFS_LOG_FLAG_XM(AllocBlock,    6)			\
			SWFS_LOG_FLAG_XM(ReadBlock,     7)			\
			SWFS_LOG_FLAG_XM(WriteBlock,    8)			\
			SWFS_LOG_FLAG_XM(DriverOp,      9)			\
			SWFS_LOG_FLAG_XM(RawDriverBytes, 30)		\
			SWFS_LOG_FLAG_XM(ForceFileFlush, 31)


#define SWFS_LOG_FLAG_XM(name, val, ...)	static const swfs_u32 SWFSLogFlag_##name = 1 << (val);
SWFS_LOG_FLAG_XM_LIST
#undef SWFS_LOG_FLAG_XM

#define SWFS_SIMPLE_FILE_INFO

typedef struct {
	swfs_u64		time_create;
#	ifndef SWFS_SIMPLE_FILE_INFO
	swfs_u64		time_read;
	swfs_u64		time_change;
#	endif
	swfs_u64		time_write;

	swfs_u64		size;
	swfs_u64		attrs;
} SWFSFInfo;

typedef struct SWFSFile {
	swfs_utf8 *			path;
	SWFSFInfo			finfo;
	swfs_u32			file_index;
	swfs_u32			mod_flags;		//SWFSNodeModFlag_* OR'd
	struct SWFSFile *	next;
} SWFSFile;

inline bool swfsDidFileChange(swfs_u32 mod_flags) {
	//basically everything except UpdateTimeRead
	return (mod_flags & (
							SWFSNodeModFlag_NewNode						|
							SWFSNodeModFlag_ChangeFlags					|
							SWFSNodeModFlag_ChangeTree					|
							SWFSNodeModFlag_Deleted						|
							SWFSNodeModFlag_UpdateTimeCreate			|
							SWFSNodeModFlag_UpdateTimeWrite				|
							SWFSNodeModFlag_UpdateTimeChange			|
							SWFSNodeModFlag_UpdateSize					|
							SWFSNodeModFlag_UpdateAttrs					|
							SWFSNodeModFlag_UpdateName
						)
			) != 0;
}

//callbacks
typedef swfs_u64				(SWFSReadFileFn)			(SWFSHandle swfs_handle, struct SWFSFile *file, void *out_buffer, swfs_u64 read_offset, swfs_u64 num_bytes_to_read); //return number of bytes read
typedef void					(SWFSFillDirBeginFn)		(SWFSHandle swfs_handle, const swfs_utf8 *path);
typedef bool					(SWFSErrorCallbackFn)		(SWFSHandle swfs_handle, SWFSResult_Enum error_code, const char *src_filename, int src_line_num, const char *error_msg);

typedef void					(SWFSNotifyWrite)			(SWFSHandle swfs_handle, SWFSFile *swfs_file);
typedef void					(SWFSNotifyCreateFileFn)	(SWFSHandle swfs_handle, SWFSFile *swfs_file);
typedef void					(SWFSNotifyMoveFn)			(SWFSHandle swfs_handle, SWFSFile *old_file, SWFSFile *new_file);
typedef void					(SWFSNotifyDeleteFn)		(SWFSHandle swfs_handle, SWFSFile *swfs_file);

typedef struct {
	SWFSReadFileFn *			read_file;
	SWFSFillDirBeginFn *		fill_dir_begin;
	SWFSErrorCallbackFn *		error_callback;

	SWFSNotifyWrite *			notify_write;
	SWFSNotifyCreateFileFn *	notify_create;
	SWFSNotifyMoveFn *			notify_move;
	SWFSNotifyDeleteFn *		notify_delete;
} SWFSCallbacks;

//optional (SWFSInit::storage): all of swfs's file writing, overridden, in one of two modes.
//
//SWFSStorageMode_Blocks - the store's own I/O.  Every read and write of the store (file data, the event log, block hashes) goes
//through storage.block instead of the swfs_datablock_* files in write_dir, the bytes exactly as swfs would have written
//them.  offset is a byte offset into the whole store: block index << 16, plus an offset inside the block for the event
//log's partial commits.  The store has no size: a write may land at any offset, and anything never written reads as
//zeros (a sparse file, or a blob store that returns zeros for what it does not hold, does both).  A new store is one
//nothing was ever written to.
//  read		num_bytes at offset into buff, all of them: zeros for whatever was never written
//  write		num_bytes from buff at offset
//  flush		everything written so far made durable
//
//SWFSStorageMode_Files - one ordinary file per volume file, through storage.file.  The I/O is block mode's, addressed by
//file instead of by store offset: everything swfs keeps about the volume - the tree, names, sizes, which blocks of a file
//hold data - is in the event log, and a file's data is at root_path\<n>, n its node index in decimal, block i of it at byte
//offset i << 16.  swfs writes whole 64 KiB blocks, so a file in root_path is at least its volume file's size rounded up to
//a block, and a truncate does not shorten it: the bytes past the new size are what a crash may still need, as block mode
//keeps a freed block until the event that frees it is durable.  The file is removed once its node is freed and that is
//durable.  Directories have no file and no directory is made: root_path has to exist.  path is root_path, the platform's
//separator and the number.
//The event log is a byte stream of its own, never a file.  After a crash root_path may hold files no node has any more
//(the free was durable, the removal not).  They are harmless: swfs only reads the blocks the log says it wrote, and a node
//that gets the number again writes over them.
//  read		num_bytes at offset into buff, all of them; zeros past the end of the file, or with no file
//  write		num_bytes from buff at offset; creates the file if it does not exist, a write past its end extends it
//  flush		the file's data made durable; true if there is no file
//  remove		the file; true if there is none
//  write_event	num_bytes appended to the event log, durable when it returns
//  read_events	at swfsInit(): the event log back, in order - exactly the bytes write_event was given, keeping them is the
//				client's responsibility.  Each call sets *out_bytes and *out_num_bytes to the bytes of one or more whole
//				write_event calls (all of them at once is fine: a file mapped or read whole), in the client's memory until
//				the next call; *out_num_bytes 0 at the end.  A write_event a crash cut short may be handed back as it is
//
//Either mode: called from swfs's io threads (SWFSInit::num_write_threads), synchronously, so they have to be thread-safe.
#define SWFS_STORAGE_RETRY_FOREVER	0xFFFFFFFFu

typedef enum {
	SWFSStorageMode_Blocks,
	SWFSStorageMode_Files,
} SWFSStorageMode_Enum;

typedef struct {
	SWFSStorageMode_Enum		mode;		//which of block and file below is set
	void *						user_data;
	union {
		struct {
			bool	(*read)		(SWFSHandle swfs_handle, void *user_data, swfs_u64 offset, void *buff, swfs_u32 num_bytes);
			bool	(*write)	(SWFSHandle swfs_handle, void *user_data, swfs_u64 offset, const void *buff, swfs_u32 num_bytes);
			bool	(*flush)	(SWFSHandle swfs_handle, void *user_data);
		} block;
		struct {
			const swfs_utf8 *	root_path;
			bool	(*read)			(SWFSHandle swfs_handle, void *user_data, const swfs_utf8 *path, swfs_u64 offset, void *buff, swfs_u32 num_bytes);
			bool	(*write)		(SWFSHandle swfs_handle, void *user_data, const swfs_utf8 *path, swfs_u64 offset, const void *buff, swfs_u32 num_bytes);
			bool	(*flush)		(SWFSHandle swfs_handle, void *user_data, const swfs_utf8 *path);
			bool	(*remove)		(SWFSHandle swfs_handle, void *user_data, const swfs_utf8 *path);
			bool	(*write_event)	(SWFSHandle swfs_handle, void *user_data, const void *bytes, swfs_u32 num_bytes);
			bool	(*read_events)	(SWFSHandle swfs_handle, void *user_data, const void **out_bytes, swfs_u32 *out_num_bytes);
		} file;
	};
} SWFSStorage;

static const swfs_u32 SWFSInitFlag_IncludeInspection				= 1;
static const swfs_u32 SWFSInitFlag_DontRunDriver					= 2;	//used for tools that don't want to mount a drive but want it loaded
static const swfs_u32 SWFSInitFlag_ReadOnly							= 4;	//drive is mounted but no writes can happen.  No backing store is created
static const swfs_u32 SWFSInitFlag_TryBackupBlockOnDataCorruption	= 8;	//a write was corrupted.  BACKUP swfs_datablock_* files before using this
static const swfs_u32 SWFSInitFlag_BlockHashes						= 16;	//keep an XXH3-64 hash of every data block and check it on every read (src/state/block_hashes.inl).
																			//Honoured only when the store is CREATED: a store has hashes for its whole life or never does.  Set
																			//on an existing store without them, or left off for one that has them, it logs a warning and the
																			//store's own setting is used

//How much of a state table's address space is committed at a time, as lg2 of bytes: SWFS_MIN_COMMIT_LG2 (16, 64 KiB) to
//SWFS_MAX_COMMIT_LG2 (30, 1 GiB), or 0 for the default given beside each.  Every table reserves its whole range at
//swfsInit() and commits its first step then, so a large step is commit charge from the start; a small one is more
//commits as the table grows.  The memory is only resident once touched either way
#define SWFS_MIN_COMMIT_LG2		16
#define SWFS_MAX_COMMIT_LG2		30
typedef struct {
	swfs_u32					nodes;					//20  the node table
	swfs_u32					node_metadata;			//20  names and file info
	swfs_u32					block_idx_tbl;			//20  every file's block indices
	swfs_u32					block_idx_tbl_free;		//16  the block index table's free list
	swfs_u32					free_blocks;			//20  the free block stack
	swfs_u32					backing_file_handles;	//16  a handle per data file
	swfs_u32					block_hashes;			//16  SWFSInitFlag_BlockHashes: the hashes, their blocks' states, replay's bitmap
	swfs_u32					mod_list;				//16  the modified-node list and its positions
	swfs_u32					mod_list_deletions;		//16  the deletions kept for the next thaw
	swfs_u32					name_index;				//16  the directory name index
	swfs_u32					frozen_nodes;			//22  a freeze's node table, list and copy buffers
	swfs_u32					thaw;					//22  a thaw's lists
	swfs_u32					reconcile;				//20  a thaw's source reconcile
	swfs_u32					tombstones;				//16  what went from the source while frozen (entries and names)
	swfs_u32					moved_dirs;				//16  directories moved while frozen
	swfs_u32					deferred_frees;			//16  nodes whose freeing waits for the thaw
	swfs_u32					source_aliases;			//16  renamed source files' source paths (entries and paths)
	swfs_u32					scratch;				//20  the work thread's and the client's scratch stacks
} SWFSCommitLg2;

typedef struct {
	const swfs_utf8 *			name;
	const swfs_utf8 *			write_dir;
	const swfs_utf8 *			mount_path;
	swfs_u32					flags;
	swfs_u32					log_flags;

	const swfs_utf8 *			shared_memory_name;		//optional.  Set if you want to share memory for external tools (at most 200 bytes)

	int							num_write_threads;		//data writes go through a pool of this many threads; anything below 1 is taken as 1 (synchronous)
	SWFSCallbacks				callbacks;


	//optional: the log file, "conLog.txt" in the working directory when NULL.  Logging and error_callback belong to the
	//process: the first swfsInit() opens the log, the last swfsClose() closes it, and in between the most recent
	//swfsInit()'s error_callback, log_flags and (if it was the first) log_path are in force.  swfsInit()/swfsClose()
	//can be called any number of times in a process
	const char *				log_path;

	//optional: the driver's data cache for this mount, in MiB; 0 takes the default (SWFS_DRIVER_CACHE_SIZE, 64 MiB).  Locked
	//nonpaged memory, mapped into this process too: the driver allocates it at the first read or write and gives it back
	//after 60 s without one, rounded up to whole 16 MiB slabs.  swfsInit() fails (a fatal error) when this is more than
	//half of the machine's physical memory or more than 65536 (64 GiB, the driver's 4096 slabs).  When the allocation
	//still fails (memory in use elsewhere), that read or write fails with STATUS_INSUFFICIENT_RESOURCES.
	swfs_u32					driver_cache_mb;

	//optional: the state tables' commit steps (SWFSCommitLg2 above); any left 0 take their defaults
	SWFSCommitLg2				commit_lg2;

	//optional: swfs's file writing, overridden (SWFSStorage above); NULL keeps the swfs_datablock_* files in write_dir.
	//Copied at swfsInit(); every function of the mode's struct is required
	SWFSStorage *				storage;
	//optional: how long a failing storage call is retried before it is a fatal error, in ms; 0 takes 10000,
	//SWFS_STORAGE_RETRY_FOREVER never gives up
	swfs_u32					storage_retry_ms;
} SWFSInit;

//To use: call 2x. Once with start_buff == NULL to ask for a buffer of num_bytes, and once with
//start_buff set to fill with data.  Set *out_num_buff_bytes to how many bytes were actually allocated
//and NULL on the second call
typedef swfs_ubyte *SWFSDataCBFn(SWFSHandle swfs_handle, void *user_data, swfs_ubyte *start_buff, size_t num_bytes, size_t *out_num_buff_bytes);

//API Functions
SWFS_API_FN	void		swfsInit				(SWFSInit *init, SWFSHandle *out_swfs_handle);
//Writes the driver's cached data back into the store first (clients need not FlushFileBuffers() before an unmount);
//a client still writing through the mount is given 10 s, then what it has not finished is lost
SWFS_API_FN	void		swfsClose				(SWFSHandle swfs_handle);
SWFS_API_FN bool		swfsIsBusy				(SWFSHandle swfs_handle);
//swfsFreeze() and swfsThaw() queue the work and the work thread does it: wait_ms says whether the call waits for that.
//  wait_ms < 0   no wait: true means queued, swfsIsFrozen() turns once the work is done
//  wait_ms == 0  wait until it is done, however long
//  wait_ms > 0   wait at most that many milliseconds
//With a wait, true means done: the freeze taken, or the thaw over and the driver caught up with it (swfsIsFrozen() false).
//false is one of two things:
//  refused       nothing was queued and nothing will happen (already frozen or a freeze queued, the driver's writes not
//                getting through, ...); there is no wait
//  not done yet  queued, but the wait ended first: the time ran out, the store is closing (or has no driver attached),
//                or the call was made on swfs's own work thread (a callback), which a wait would deadlock.  The queued
//                work is still done: swfsIsFrozen() tells when
//A freeze's snapshot is what applications had completed at the call: they are held from the call until it is taken
SWFS_API_FN	bool		swfsFreeze				(SWFSHandle swfs_handle, SWFSDataCBFn *cb_fn, void *cb_user_data, int wait_ms);		//cb_fn is called once with a linked list of SWFSFile *.  Set mod_flags bit SWFSNodeModFlag_DontFreeze for each file you don't want included in a freeze. SWFS owns the data, do not free it
//files: a linked list of what the client committed to the source, by path, in the source's terms (a directory has
//A named file takes the source's state and comes off the modified list.  A named directory is relisted against the
//source: its unmodified files are refreshed, names the source added or removed come and go - but nothing below it is
//committed: a modified file in it, held by the freeze or not, stays modified until a thaw names the file itself.  A name the user
//touched while frozen keeps the swfs version.  NULL names nothing: the freeze ends and the list is as it was.
SWFS_API_FN	bool		swfsThaw				(SWFSHandle swfs_handle, const SWFSFile *files, int wait_ms);	//wait_ms: as swfsFreeze()'s
SWFS_API_FN bool		swfsIsFrozen			(SWFSHandle swfs_handle);
SWFS_API_FN swfs_u32	swfsGetFrozenFileList	(SWFSHandle swfs_handle, SWFSDataCBFn *cb_fn, void *cb_user_data);
//num_bytes of a frozen file's snapshot from offset, both multiples of 2^SWFS_BUFF_ALIGN_LG2 (SWFSResult_RangeNotAligned
//otherwise), the range clamped to the file - so the last block's read delivers what is left of the file.  The read moves
//whole blocks: cb_fn's first call asks for the clamped range rounded up to one (aligned, as every IO buffer), the second
//hands back the bytes delivered at its start.  Returns that count: 0 at or past the end, or for a file not frozen or
//frozen without bytes of its own (changed only in name or attributes: its bytes are the source's)
SWFS_API_FN size_t		swfsReadFrozenFile		(SWFSHandle swfs_handle, const swfs_utf8 *path, swfs_u16 path_len, swfs_u64 offset, size_t num_bytes, SWFSDataCBFn *cb_fn);
SWFS_API_FN swfs_u32	swfsGetModifiedFileList	(SWFSHandle swfs_handle, SWFSDataCBFn *cb_fn, void *cb_user_data);
SWFS_API_FN bool		swfsResetCache			(SWFSHandle swfs_handle);
SWFS_API_FN bool		swfsFillDirAddFiles		(SWFSHandle swfs_handle, const SWFSFile *first_file);		//must be called from fill_dir() callback

SWFS_API_END

#endif //SWFS_H_


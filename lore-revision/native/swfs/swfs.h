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
#define SWFS_MAX_FILENAME_LENGTH	255

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
	SWFSResult_ThawPathConflict,				//a swfsThaw() path runs through an existing file ("x\y" with x a file), or names one thing as both a file and a directory.  That path is skipped: existing nodes are never replaced
	SWFSResult_RangeNotAligned,					//swfsReadFrozenFile()'s offset and num_bytes must be multiples of 2^SWFS_BUFF_ALIGN_LG2 bytes (64kb)
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

static const swfs_u32 SWFSInitFlag_IncludeInspection				= 1;
static const swfs_u32 SWFSInitFlag_DontRunDriver					= 2;	//used for tools that don't want to mount a drive but want it loaded
static const swfs_u32 SWFSInitFlag_ReadOnly							= 4;	//drive is mounted but no writes can happen.  No backing store is created
static const swfs_u32 SWFSInitFlag_TryBackupBlockOnDataCorruption	= 8;	//a write was corrupted.  BACKUP swfs_datablock_* files before using this

typedef struct {
	const swfs_utf8 *			name;
	const swfs_utf8 *			write_dir;
	const swfs_utf8 *			mount_path;
	swfs_u32					flags;
	swfs_u32					log_flags;

	const swfs_utf8 *			shared_memory_name;		//optional.  Set if you want to share memory for external tools

	int							num_write_threads;		//data writes go through a pool of this many threads; anything below 1 is taken as 1 (synchronous)
	SWFSCallbacks				callbacks;


	//optional: the log file, "conLog.txt" in the working directory when NULL.  Logging and error_callback belong to the
	//process: the first swfsInit() opens the log, the last swfsClose() closes it, and in between the most recent
	//swfsInit()'s error_callback, log_flags and (if it was the first) log_path are in force.  swfsInit()/swfsClose()
	//can be called any number of times in a process
	const char *				log_path;
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
//Also writes the driver's cached data back first, so the list and the snapshot include every write that completed
//before the call (same 10 s bound as swfsClose() against a client that keeps writing)
SWFS_API_FN	bool		swfsFreeze				(SWFSHandle swfs_handle, SWFSDataCBFn *cb_fn, void *cb_user_data);		//cb_fn is called once with a linked list of SWFSFile *.  Set mod_flags bit SWFSNodeModFlag_DontFreeze for each file you don't want included in a freeze. SWFS owns the data, do not free it
//files: a linked list of what the client committed to the source, by path, in the source's terms (a directory has
//FILE_ATTRIBUTE_DIRECTORY in finfo.attrs; a deletion is named by the path swfsGetModifiedFileList() reported it under).
//A named file takes the source's state and comes off the modified list.  A named directory is relisted against the
//source: its unmodified files are refreshed, names the source added or removed come and go - but nothing below it is
//committed: a modified file in it, held by the freeze or not, stays modified until a thaw names the file itself.  A name the user
//touched while frozen keeps the swfs version.  NULL names nothing: the freeze ends and the list is as it was
SWFS_API_FN	bool		swfsThaw				(SWFSHandle swfs_handle, const SWFSFile *files);
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


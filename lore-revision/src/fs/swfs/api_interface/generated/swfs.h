#ifndef SWFS_H_
#define SWFS_H_

//API for clients using the library

typedef unsigned char		swfs_ubyte;
typedef char				swfs_utf8;
typedef wchar_t				swfs_utf16;
typedef unsigned char		swfs_u8;
typedef unsigned short		swfs_u16;
typedef unsigned int		swfs_u32;
typedef unsigned long long	swfs_u64;
typedef void *				SWFSHandle;

#define SWFS_API_FN				__declspec(dllexport)

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

//Error codes under SWFSResult_FirstError are HRESULTS
typedef enum {
	SWFSResult_Success,

	SWFSResult_FirstError		= 0x80000000,

	SWFSResult_FileNotFound,
} SWFSResult_Enum;

#define SWFS_BUFF_ALIGN_LG2 16			//all direct file access buffers must be aligned to 64kb

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
										

#define NODE_MOD_FLAG_XM(name, flag_char, bit,  ...)	static const swfs_u16 SWFSNodeModFlag_##name = 1 << (bit);
NODE_MOD_FLAG_XM_LIST
#undef NODE_MOD_FLAG_XM

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


#define SWFS_LOG_FLAG_XM(name, val, ...)	static const swfs_u32 SWFSLogFlag##name = 1 << (val);
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
	swfs_u16			mod_flags;		//SWFSNodeModFlag_* OR'd
	struct SWFSFile *	next;
} SWFSFile;

typedef enum {
	SWFSInstallResult_Success					= 0,
	SWFSInstallResult_NoChange					= 1,
	//errors
	SWFSInstallResult_InvalidArgs				= -1,
	SWFSInstallResult_InvalidPath				= -2,
	SWFSInstallResult_CopyFile					= -3,
	SWFSInstallResult_OpenSCManager				= -4,
	SWFSInstallResult_ChangeService				= -5,
	SWFSInstallResult_CreateService				= -6,
	SWFSInstallResult_QueryService				= -7,
	SWFSInstallResult_RequiresReboot			= -8,
	SWFSInstallResult_UnableToReplaceOldDriver	= -9,
	SWFSInstallResult_StartService				= -10,
} SWFSInstallResult_Enum;

inline bool swfsDidFileChange(swfs_u16 mod_flags) {
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

static const swfs_u32 SWFSInitFlag_IncludeInspection	= 1;
static const swfs_u32 SWFSInitFlag_DontRunDriver		= 2;	//used for tools that don't want to mount a drive but want it loaded
static const swfs_u32 SWFSInitFlag_ReadOnly				= 4;	//drive is mounted but no writes can happen.  No backing store is created

typedef struct {
	const swfs_utf8 *			name;
	const swfs_utf8 *			write_dir;
	const swfs_utf8 *			mount_path;
	swfs_u32					flags;
	swfs_u32					log_flags;

	const swfs_utf8 *			shared_memory_name;		//optional.  Set if you want to share memory for external tools

	int							num_write_threads;		//default = 2
	SWFSCallbacks				callbacks;

	const char *				driver_install_path;	//Defaults to GetSystemDirectory()\drivers\venfs.sys (likely C:\Windows\System32\drivers\venfs.sys)

	const char *				log_path;
} SWFSInit;

typedef swfs_ubyte *SWFSDataCBFn(SWFSHandle swfs_handle, void *user_data, swfs_ubyte *start_buff, size_t num_bytes, size_t *out_num_buff_bytes);

//API Functions
SWFS_API_FN	void		swfsInit				(SWFSInit *init, SWFSHandle *out_swfs_handle);
SWFS_API_FN	void		swfsClose				(SWFSHandle swfs_handle);
SWFS_API_FN bool		swfsIsBusy				(SWFSHandle swfs_handle);
SWFS_API_FN	bool		swfsFreeze				(SWFSHandle swfs_handle, const swfs_utf8 *paths, swfs_u32 num_paths);
SWFS_API_FN	bool		swfsThaw				(SWFSHandle swfs_handle, const SWFSFile *files);
SWFS_API_FN bool		swfsIsFrozen			(SWFSHandle swfs_handle);
SWFS_API_FN swfs_u32	swfsGetFrozenFileList	(SWFSHandle swfs_handle, SWFSDataCBFn *cb_fn, void *cb_user_data);
SWFS_API_FN size_t		swfsReadFrozenFile		(SWFSHandle swfs_handle, const swfs_utf8 *path, swfs_u16 path_len, SWFSDataCBFn *cb_fn);
SWFS_API_FN swfs_u32	swfsGetModifiedFileList	(SWFSHandle swfs_handle, SWFSDataCBFn *cb_fn, void *cb_user_data);
SWFS_API_FN bool		swfsResetCache			(SWFSHandle swfs_handle);
SWFS_API_FN bool		swfsFillDirAddFiles		(SWFSHandle swfs_handle, const SWFSFile *first_file);		//must be called from fill_dir() callback

SWFS_API_END

#endif //SWFS_H_


/*
 * Copyright 2026 NXP
 * All rights reserved.
 *
 * SPDX-License-Identifier: BSD-3-Clause
 *
 */

use bincode::serialize;
use log::{info, trace};
use nix::libc;
use std::fs;
use std::fs::File;
use std::fs::ReadDir;
use std::io::Error;
use std::io::Seek;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::OpenOptionsExt;
mod io;
mod map;
mod msgs;

const MAX_CONTENT_SIZE: usize = 200;
const RESULT_DO_NOT_SEND_RESPONSE: i32 = 0xAAAA;

fn str_from_u8_nul_utf8(utf8_src: &[u8]) -> &str {
    let nul_range_end = utf8_src
        .iter()
        .position(|&c| c == b'\0')
        .unwrap_or(utf8_src.len()); // default to length if no `\0` present
    ::std::str::from_utf8(&utf8_src[0..nul_range_end]).unwrap_or("")
}

pub struct Rpmsgfs {
    rpmsgfs_io: io::Io,
    files: map::Map<File>,
    directories: map::Map<ReadDir>,
}

impl Rpmsgfs {
    pub fn new(device_filename: &std::path::Path) -> Rpmsgfs {
        Rpmsgfs {
            rpmsgfs_io: io::Io::new(device_filename),
            files: map::Map::new(),
            directories: map::Map::new(),
        }
    }

    fn open(&mut self, _header: &msgs::Header, data: &[u8]) -> Result<(i32, Vec<u8>), Error> {
        let open_data: msgs::Open = bincode::deserialize(&data).unwrap();

        let path_offset = std::mem::size_of::<msgs::Open>();
        let path = str_from_u8_nul_utf8(&data[path_offset..]);
        info!(
            "open {:?}, mode:{:o}, flags:0x{:x}",
            path, open_data.mode, open_data.flags
        );

        let custom_flags: i32 = match open_data.flags & msgs::O_NOFOLLOW {
            msgs::O_NOFOLLOW => libc::O_NOFOLLOW,
            _ => 0,
        } | match open_data.flags & msgs::O_EXCL {
            msgs::O_EXCL => libc::O_EXCL,
            _ => 0,
        } | match open_data.flags & msgs::O_NONBLOCK {
            msgs::O_NONBLOCK => libc::O_NONBLOCK,
            _ => 0,
        } | match open_data.flags & msgs::O_SYNC {
            msgs::O_SYNC => libc::O_SYNC,
            _ => 0,
        } | match open_data.flags & msgs::O_DIRECT {
            msgs::O_DIRECT => libc::O_DIRECT,
            _ => 0,
        } | match open_data.flags & msgs::O_DIRECTORY {
            msgs::O_DIRECTORY => libc::O_DIRECTORY,
            _ => 0,
        } | match open_data.flags & msgs::O_LARGEFILE {
            msgs::O_LARGEFILE => libc::O_LARGEFILE,
            _ => 0,
        } | match open_data.flags & msgs::O_NOATIME {
            msgs::O_NOATIME => libc::O_NOATIME,
            _ => 0,
        };

        let file = std::fs::OpenOptions::new()
            .read((open_data.flags & msgs::O_READ) == msgs::O_READ)
            .write((open_data.flags & msgs::O_WRITE) == msgs::O_WRITE)
            .create((open_data.flags & msgs::O_CREAT) == msgs::O_CREAT)
            .append((open_data.flags & msgs::O_APPEND) == msgs::O_APPEND)
            .truncate((open_data.flags & msgs::O_TRUNC) == msgs::O_TRUNC)
            .custom_flags(custom_flags)
            .mode(open_data.mode)
            .open(path)?;

        Ok((self.files.add(file, path.to_string()), vec![]))
    }

    fn close(&mut self, _header: &msgs::Header, data: &[u8]) -> Result<(i32, Vec<u8>), Error> {
        let fd: i32 = bincode::deserialize(&data).unwrap();
        info!("close {:}", fd);

        self.directories.remove(fd)?;
        Ok((0, vec![]))
    }

    fn read(&mut self, header: &msgs::Header, data: &[u8]) -> Result<(i32, Vec<u8>), Error> {
        let read_data: msgs::FileContent = bincode::deserialize(&data).unwrap();
        info!("read from {:}", read_data.fd);

        let (file, _) = self.files.get_mut(read_data.fd)?;

        let mut pending_bytes = read_data.content_size as usize;
        while pending_bytes > 0 {
            trace!("pending_bytes = {:}", pending_bytes);
            let mut buf = vec![];
            let max_chunk_size = match pending_bytes < MAX_CONTENT_SIZE {
                true => pending_bytes,
                false => MAX_CONTENT_SIZE,
            };
            let mut chunk = file.take(max_chunk_size as u64);
            trace!("buf len = {:}", buf.len());
            let bytes_read = chunk.read_to_end(&mut buf)?;
            trace!("size = {:}", bytes_read);
            trace!("{:?}", buf);
            let response = [serialize(&read_data).unwrap(), buf].concat();

            pending_bytes = pending_bytes - bytes_read;

            // if no bytes read then end the read process
            if bytes_read == 0 {
                pending_bytes = 0;
            }

            self.rpmsgfs_io
                .send_response(header, bytes_read as i32, response)
                .expect("cannot send read response");
        }
        Ok((RESULT_DO_NOT_SEND_RESPONSE, vec![]))
    }

    fn write(&mut self, header: &msgs::Header, data: &[u8]) -> Result<(i32, Vec<u8>), Error> {
        let write_data: msgs::FileContent = bincode::deserialize(&data).unwrap();

        let content_offset = std::mem::size_of::<msgs::FileContent>();
        let content = &data[content_offset..(content_offset + (write_data.content_size as usize))];

        let (file, _) = self.files.get_mut(write_data.fd)?;
        info!("write to {:}", &write_data.fd);

        let size = file.write(content)?;

        if header.cookie != 0 {
            Ok((size as i32, vec![]))
        } else {
            Ok((RESULT_DO_NOT_SEND_RESPONSE, vec![]))
        }
    }

    fn seek(&mut self, _header: &msgs::Header, data: &[u8]) -> Result<(i32, Vec<u8>), Error> {
        let seek_data: msgs::Seek = bincode::deserialize(&data).unwrap();
        info!("seek {:}", seek_data.fd);

        let (file, _) = self.files.get_mut(seek_data.fd)?;

        file.seek(match seek_data.whence {
            0 => std::io::SeekFrom::Start(seek_data.offset as u64),
            2 => std::io::SeekFrom::End(seek_data.offset as i64),
            _ => std::io::SeekFrom::Current(seek_data.offset as i64),
        })?;
        Ok((0, vec![]))
    }

    fn sync(&mut self, _header: &msgs::Header, data: &[u8]) -> Result<(i32, Vec<u8>), Error> {
        let fd: i32 = bincode::deserialize(&data).unwrap();
        info!("sync {:}", fd);

        let (file, _) = self.files.get_mut(fd)?;

        file.sync_all()?;
        Ok((0, vec![]))
    }

    fn ftruncate(&mut self, _header: &msgs::Header, data: &[u8]) -> Result<(i32, Vec<u8>), Error> {
        let ftruncate_data: msgs::FTruncate = bincode::deserialize(&data).unwrap();
        info!("ftruncate {:}", ftruncate_data.fd);

        let (file, _) = self.files.get_mut(ftruncate_data.fd)?;

        file.set_len(ftruncate_data.lenght as u64)?;
        Ok((0, vec![]))
    }

    fn opendir_helper(&mut self, path: &String) -> Result<(i32, Vec<u8>), Error> {
        let dir = fs::read_dir(path)?;
        Ok((self.directories.add(dir, path.to_string()), vec![]))
    }

    fn opendir(&mut self, _header: &msgs::Header, data: &[u8]) -> Result<(i32, Vec<u8>), Error> {
        let path = str_from_u8_nul_utf8(&data);
        info!("opendir {:?}", path);

        self.opendir_helper(&path.to_string())
    }

    fn convert_file_type(dir_entry: &std::fs::DirEntry) -> u32 {
        match dir_entry.file_type() {
            Ok(file_type) => {
                if file_type.is_file() {
                    msgs::DT_REG
                } else if file_type.is_char_device() {
                    msgs::DT_CHR
                } else if file_type.is_block_device() {
                    msgs::DT_BLK
                } else if file_type.is_dir() {
                    msgs::DT_DIR
                } else if file_type.is_symlink() {
                    msgs::DT_LNK
                } else if file_type.is_fifo() {
                    msgs::DT_FIFO
                } else if file_type.is_socket() {
                    msgs::DT_SOCK
                } else {
                    msgs::DT_UNKNOWN
                }
            }
            Err(_) => msgs::DT_UNKNOWN,
        }
    }

    fn readdir(&mut self, _header: &msgs::Header, data: &[u8]) -> Result<(i32, Vec<u8>), Error> {
        let readdir_data: msgs::ReadDir = bincode::deserialize(&data).unwrap();

        info!("readdir {:}", &readdir_data.dir_id);

        let (dir, _) = self.directories.get_mut(readdir_data.dir_id)?;
        match dir.next() {
            Some(item) => {
                let dir_entry = item?;
                let readdir_response = msgs::ReadDir {
                    dir_id: readdir_data.dir_id,
                    item_type: Self::convert_file_type(&dir_entry),
                };
                let filename = dir_entry.file_name().into_vec();
                let response = [serialize(&readdir_response).unwrap(), filename, vec![0]].concat();

                Ok((0, response))
            }
            None => Err(Error::from_raw_os_error(libc::ENOENT)),
        }
    }

    fn rewinddir(&mut self, _header: &msgs::Header, data: &[u8]) -> Result<(i32, Vec<u8>), Error> {
        let dir_id: i32 = bincode::deserialize(&data).unwrap();
        info!("rewinddir {:}", dir_id);

        /* Rewind is not possible so just remove and reopen dir */
        let (_, path) = self.directories.remove(dir_id)?;
        self.opendir_helper(&path)
    }

    fn closedir(&mut self, _header: &msgs::Header, data: &[u8]) -> Result<(i32, Vec<u8>), Error> {
        let dir_id: i32 = bincode::deserialize(&data).unwrap();
        info!("closedir {:}", dir_id);

        self.directories.remove(dir_id)?;
        Ok((0, vec![]))
    }

    fn statfs(&mut self, _header: &msgs::Header, data: &[u8]) -> Result<(i32, Vec<u8>), Error> {
        let path_offset = std::mem::size_of::<msgs::Statfs>();
        let path = str_from_u8_nul_utf8(&data[path_offset..]);

        info!("statfs {:?}", path);

        match nix::sys::statfs::statfs(if path.is_empty() { "/" } else { path }) {
            Ok(statfs) => {
                let statfs_data = msgs::Statfs {
                    fstype: u32::try_from(statfs.filesystem_type().0).unwrap_or(0),
                    reserved: 0,
                    namelen: statfs.maximum_name_length() as i64,
                    bsize: statfs.block_size() as i64,
                    blocks: statfs.blocks(),
                    bfree: statfs.blocks_free(),
                    bavail: statfs.blocks_available(),
                    files: statfs.files(),
                    ffree: statfs.files_free(),
                };
                Ok((0, serialize(&statfs_data).unwrap()))
            }
            Err(e) => Err(Error::from_raw_os_error(e as i32)),
        }
    }

    fn stat_helper(path: &str) -> Result<(i32, Vec<u8>), Error> {
        let stat_result = nix::sys::stat::stat(path)?;
        let stat_response = msgs::Stat {
            dev: stat_result.st_dev as u32,
            mode: stat_result.st_mode,
            rdev: stat_result.st_rdev as u32,
            ino: stat_result.st_ino as u16,
            nlink: stat_result.st_nlink as u16,
            size: stat_result.st_size as i64,
            atim_sec: stat_result.st_atime as i64,
            atim_nsec: stat_result.st_atime_nsec as i64,
            mtim_sec: stat_result.st_mtime as i64,
            mtim_nsec: stat_result.st_mtime_nsec as i64,
            ctim_sec: stat_result.st_ctime as i64,
            ctim_nsec: stat_result.st_ctime_nsec as i64,
            blocks: stat_result.st_blocks as u64,
            uid: stat_result.st_uid as i16,
            gid: stat_result.st_gid as i16,
            blksize: stat_result.st_blksize as i16,
            reserved: 0,
        };
        Ok((0, serialize(&stat_response).unwrap()))
    }

    fn fstat(&mut self, _header: &msgs::Header, data: &[u8]) -> Result<(i32, Vec<u8>), Error> {
        let path_offset = std::mem::size_of::<msgs::Stat>();
        let file_descriptor: i32 = bincode::deserialize(&data[path_offset..]).unwrap();

        let (_, path) = self.files.get_mut(file_descriptor)?;
        info!("stat {:?}", path);

        Self::stat_helper(path)
    }

    fn stat(&mut self, _header: &msgs::Header, data: &[u8]) -> Result<(i32, Vec<u8>), Error> {
        let path_offset = std::mem::size_of::<msgs::Stat>();
        let path = str_from_u8_nul_utf8(&data[path_offset..]);
        info!("stat {:?}", path);

        Self::stat_helper(path)
    }

    fn chstat_helper(path: &str, chstat_data: &msgs::Chstat) -> Result<(), Error> {
        let mode = nix::sys::stat::Mode::from_bits(chstat_data.stat.mode)
            .unwrap_or(nix::sys::stat::Mode::empty());
        nix::sys::stat::fchmodat(
            nix::fcntl::AT_FDCWD,
            path,
            mode,
            nix::sys::stat::FchmodatFlags::FollowSymlink,
        )?;

        let atime = nix::sys::time::TimeSpec::new(
            chstat_data.stat.atim_sec as nix::sys::time::time_t,
            chstat_data.stat.atim_nsec as nix::sys::time::time_t,
        );
        let mtime = nix::sys::time::TimeSpec::new(
            chstat_data.stat.mtim_sec as nix::sys::time::time_t,
            chstat_data.stat.mtim_nsec as nix::sys::time::time_t,
        );
        nix::sys::stat::utimensat(
            nix::fcntl::AT_FDCWD,
            path,
            &atime,
            &mtime,
            nix::sys::stat::UtimensatFlags::FollowSymlink,
        )?;
        Ok(())
    }

    fn fchstat(&mut self, _header: &msgs::Header, data: &[u8]) -> Result<(i32, Vec<u8>), Error> {
        let chstat_data: msgs::Chstat = bincode::deserialize(&data).unwrap();
        let path_offset = std::mem::size_of::<msgs::Chstat>();
        let file_descriptor: i32 = bincode::deserialize(&data[path_offset..]).unwrap();
        info!("fchstat {:}", file_descriptor);

        let (_, path) = self.files.get_mut(file_descriptor)?;
        Self::chstat_helper(path, &chstat_data)?;
        Ok((0, vec![]))
    }

    fn chstat(&mut self, _header: &msgs::Header, data: &[u8]) -> Result<(i32, Vec<u8>), Error> {
        let chstat_data: msgs::Chstat = bincode::deserialize(&data).unwrap();
        let path_offset = std::mem::size_of::<msgs::Chstat>();
        let path = str_from_u8_nul_utf8(&data[path_offset..]);
        info!("chstat {:?}", path);

        Self::chstat_helper(path, &chstat_data)?;
        Ok((0, vec![]))
    }

    fn unlink(&mut self, _header: &msgs::Header, data: &[u8]) -> Result<(i32, Vec<u8>), Error> {
        let path = str_from_u8_nul_utf8(&data);
        info!("unlink {:?}", path);

        fs::remove_file(path)?;
        Ok((0, vec![]))
    }

    fn mkdir(&mut self, _header: &msgs::Header, data: &[u8]) -> Result<(i32, Vec<u8>), Error> {
        let readdir_data: msgs::MkDir = bincode::deserialize(&data).unwrap();
        let path_offset = std::mem::size_of::<msgs::MkDir>();
        let path = str_from_u8_nul_utf8(&data[path_offset..]);
        info!("mkdir {:?}", path);

        std::fs::DirBuilder::new()
            .mode(readdir_data.mode)
            .create(path)?;
        Ok((0, vec![]))
    }

    fn rmdir(&mut self, _header: &msgs::Header, data: &[u8]) -> Result<(i32, Vec<u8>), Error> {
        let path = str_from_u8_nul_utf8(&data);
        info!("rmdir {:?}", path);

        fs::remove_dir(path)?;
        Ok((0, vec![]))
    }

    fn rename(&mut self, _header: &msgs::Header, data: &[u8]) -> Result<(i32, Vec<u8>), Error> {
        let path_from = str_from_u8_nul_utf8(&data);
        let path_to_offset = (path_from.len() + 1 + 0x7) & !0x7;
        let path_to = str_from_u8_nul_utf8(&data[path_to_offset..]);
        info!("rename {:?}->{:?}", path_from, path_to);

        fs::rename(path_from, path_to)?;
        Ok((0, vec![]))
    }

    pub fn process_command(&mut self) {
        let (buf, size) = self.rpmsgfs_io.read_packet();
        info!("Recv msg: {:?}", &buf[..size]);

        let header: msgs::Header = bincode::deserialize(&buf).unwrap();
        trace!("cmd:{:} cookie:0x{:x}", header.command, header.cookie);
        let data_offset = std::mem::size_of::<msgs::Header>();
        let data = &buf[data_offset..];
        let result = match header.command {
            msgs::CMD_OPEN => self.open(&header, &data),
            msgs::CMD_CLOSE => self.close(&header, &data),
            msgs::CMD_READ => self.read(&header, &data),
            msgs::CMD_WRITE => self.write(&header, &data),
            msgs::CMD_SEEK => self.seek(&header, &data),
            //msgs::CMD_IOCTL => self.ioctl(&header, &data),
            msgs::CMD_SYNC => self.sync(&header, &data),
            //msgs::CMD_DUP => self.dup(&header, &data),
            msgs::CMD_FSTAT => self.fstat(&header, &data),
            msgs::CMD_FTRUNCATE => self.ftruncate(&header, &data),
            msgs::CMD_OPENDIR => self.opendir(&header, &data),
            msgs::CMD_READDIR => self.readdir(&header, &data),
            msgs::CMD_REWINDDIR => self.rewinddir(&header, &data),
            msgs::CMD_CLOSEDIR => self.closedir(&header, &data),
            msgs::CMD_STATFS => self.statfs(&header, &data),
            msgs::CMD_UNLINK => self.unlink(&header, &data),
            msgs::CMD_MKDIR => self.mkdir(&header, &data),
            msgs::CMD_RMDIR => self.rmdir(&header, &data),
            msgs::CMD_RENAME => self.rename(&header, &data),
            msgs::CMD_STAT => self.stat(&header, &data),
            msgs::CMD_FCHSTAT => self.fchstat(&header, &data),
            msgs::CMD_CHSTAT => self.chstat(&header, &data),
            _ => Err(Error::from_raw_os_error(-libc::ENOTSUP)),
        };
        match result {
            Ok((result, response_data)) => {
                if result != RESULT_DO_NOT_SEND_RESPONSE {
                    self.rpmsgfs_io
                        .send_response(&header, result, response_data)
                        .expect("Cannot send rename response to rpmsg characted device");
                } else {
                }
            }

            Err(e) => {
                self.rpmsgfs_io
                    .send_response(&header, -e.raw_os_error().unwrap(), vec![])
                    .expect("Cannot send rename response to rpmsg characted device");
            }
        };
    }
}

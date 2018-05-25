//
// Copyright (c) 2018, The MesaLock Linux Project Contributors
// All rights reserved.
// 
// This work is licensed under the terms of the BSD 3-Clause License.
// For a copy, see the LICENSE file.
//

use super::{Result, UtilRead, UtilWrite, UtilSetup};
use util;

use clap::{Arg, ArgGroup, AppSettings};
use std::collections::VecDeque;
use std::ffi::{OsString, OsStr};
use std::fs::File;
use std::io::{self, BufReader, BufRead, Read, Write};
use std::mem;
use std::result::Result as StdResult;
use std::path::Path;

pub const NAME: &str = "head";
pub const DESCRIPTION: &str = "Print the first N bytes or lines from a file";

const AFTER_HELP: &str = "
This utility acts as if the user provided the argument '-n 10' by default.

NUMBER may be given a multiplier suffix following the International System of Units (SI), meaning
that kB is 1000, MB is 1000^2, etc. up to YB (which is 1000^8).  If you remove the 'B' from the
suffix, the number is interpreted as its IEC equivalent (e.g. K means 1024 and M means 1024^2).
Providing the suffix 'b' is equivalent to multiplying NUMBER by 512.

Please note that the maximum value for NUMBER is the maximum value of your platform's native
integer type (so a 64-bit number on 64-bit platforms).  Therefore, some suffixes may not work at
all on your system.
";

enum Mode {
    Bytes((usize, bool)),
    Lines((usize, bool)),
}

struct Options {
    method: Mode,
    previous_printed: bool,
}

pub fn execute<I, O, E, T, U>(setup: &mut UtilSetup<I, O, E>, args: T) -> Result<()>
where
    I: for<'a> UtilRead<'a>,
    O: for<'a> UtilWrite<'a>,
    E: for<'a> UtilWrite<'a>,
    T: Iterator<Item = U>,
    U: Into<OsString> + Clone,
{
    // TODO: check for obsolete arg style (e.g. head -5 file)
    let matches = {
        let app = util_app!("head", setup)
                    .setting(AppSettings::AllowLeadingHyphen)
                    .after_help(AFTER_HELP)
                    .group(ArgGroup::with_name("mode")
                            .arg("bytes")
                            .arg("lines"))
                    .arg(Arg::with_name("bytes")
                            .short("c")
                            .long("bytes")
                            .takes_value(true)
                            .value_name("NUMBER")
                            .validator_os(is_valid_num)
                            .help("Print the first NUMBER bytes if NUMBER is positive; otherwise print all but the last NUMBER bytes"))
                    .arg(Arg::with_name("lines")
                            .short("n")
                            .long("lines")
                            .takes_value(true)
                            .value_name("NUMBER")
                            .validator_os(is_valid_num)
                            .help("Print the first NUMBER lines if NUMBER is positive; otherwise print all but the last NUMBER lines"))
                    .arg(Arg::with_name("quiet")
                            .short("q")
                            .long("quiet")
                            .visible_alias("silent")
                            .help("Never print file headers"))
                    .arg(Arg::with_name("verbose")
                            .short("v")
                            .long("verbose")
                            .overrides_with("quiet")
                            .help("Always print file headers"))
                    .arg(Arg::with_name("FILES")
                            .index(1)
                            .multiple(true));
    
        app.get_matches_from_safe(args)?
    };

    let verbose = matches.is_present("verbose");
    let quiet = matches.is_present("quiet");

    // these .unwrap()s are fine because of the validators above
    let method = if matches.is_present("bytes") {
        let num = parse_num(matches.value_of("bytes").unwrap()).unwrap();
        Mode::Bytes(num)
    } else if matches.is_present("lines") {
        let num = parse_num(matches.value_of("lines").unwrap()).unwrap();
        Mode::Lines(num)
    } else {
        // just dump the first ten lines
        Mode::Lines((10, true))
    };

    let mut options = Options {
        method: method,
        previous_printed: false,
    };

    let mut output = setup.stdout.lock_writer()?;
    if matches.is_present("FILES") {
        let mut result = Ok(());
        let mut err_stream = setup.stderr.lock_writer()?;

        let file_count = matches.occurrences_of("FILES");

        for file in matches.values_of_os("FILES").unwrap() {
            let filename = if (file_count > 1 && !quiet) || verbose {
                Some(file)
            } else {
                None
            };
            let res = if file == OsStr::new("-") {
                let filename = filename.map(|_| OsStr::new("standard input"));
                handle_stdin(&mut output, &mut setup.stdin, filename, &mut options)
            } else {
                handle_file(&mut output, file, filename, &mut options)
            };

            if let Err(mut e) = res {
                display_msg!(err_stream, "{}", e)?;
                e.err = None;
                result = Err(e);
            }
        }

        result
    } else {
        let filename = if verbose {
            Some(OsStr::new("standard input"))
        } else {
            None
        };
        handle_stdin(output, &mut setup.stdin, filename, &mut options)
    }
}

fn handle_stdin<I, O>(output: O, stdin: &mut I, filename: Option<&OsStr>, options: &mut Options) -> Result<()>
where
    I: for<'a> UtilRead<'a>,
    O: Write,
{
    let stdin = stdin.lock_reader()?;
    handle_data(output, stdin, filename, options)
}

fn handle_file<O: Write>(output: O, filename: &OsStr, disp_filename: Option<&OsStr>, options: &mut Options) -> Result<()> {
    let file = File::open(filename)?;
    let reader = BufReader::new(file);
    handle_data(output, reader, disp_filename, options)
}

fn handle_data<W, R>(mut output: W, input: R, filename: Option<&OsStr>, options: &mut Options) -> Result<()>
where
    W: Write,
    R: BufRead,
{
    if let Some(name) = filename {
        let path = Path::new(name);
        if options.previous_printed {
            writeln!(output, "\n==> {} <==", path.display())?;
        } else {
            writeln!(output, "==> {} <==", path.display())?;
            options.previous_printed = true;
        }
    }
    match options.method {
        Mode::Lines((lines, positive)) => {
            if positive {
                write_lines_forward(output, input, lines)
            } else {
                write_lines_backward(output, input, lines)
            }
        }
        Mode::Bytes((bytes, positive)) => {
            if positive {
                io::copy(&mut input.take(bytes as u64), &mut output)?;
                Ok(())
            } else {
                write_bytes_backward(output, input, bytes)
            }
        }
    }
}

fn write_lines_forward<W, R>(mut output: W, mut input: R, mut line_count: usize) -> Result<()>
where
    W: Write,
    R: BufRead,
{
    let mut buffer = vec![];
    while line_count > 0 {
        // NOTE: it would be faster to just continuously read into the buffer and then
        //       write once, but that could potentially take a lot of memory
        let count = input.read_until(b'\n', &mut buffer)?;
        if count == 0 {
            break;
        }
        output.write_all(&buffer)?;

        buffer.clear();
        line_count -= 1;
    }

    Ok(())
}

fn write_lines_backward<W, R>(mut output: W, mut input: R, mut line_count: usize) -> Result<()>
where
    W: Write,
    R: BufRead,
{
    let mut store = VecDeque::new();

    // returns true if we can just return rather than printing
    let mut read_line = |store: &mut VecDeque<_>, mut line| -> StdResult<_, io::Error> {
        if input.read_until(b'\n', &mut line)? == 0 {
            return Ok(true);
        }
        store.push_back(line);
        Ok(false)
    };

    while line_count > 0 {
        if read_line(&mut store, vec![])? {
            return Ok(());
        }
        line_count -= 1;
    }
    if !read_line(&mut store, vec![])? {
        loop {
            // this .unwrap() is fine because we always push another line into the store
            let mut line = store.pop_front().unwrap();
            output.write_all(&line)?;
            line.clear();
            if read_line(&mut store, line)? {
                break;
            }
        }
    }

    Ok(())
}

fn write_bytes_backward<W, R>(mut output: W, mut input: R, bytes: usize) -> Result<()>
where
    W: Write,
    R: BufRead,
{
    const BUF_SIZE: usize = 32 * 1024;
    
    // FIXME: if the user provides a byte count greater than the amount of memory available and
    //        the file size is also greater than the amount of memory available, this will
    //        currently exhaust memory and abort.  not sure what the best way to fix this is other
    //        than writing to a temporary file if the size is too large (but this solution comes
    //        with its own issues as well)
    let size = bytes.max(32 * 1024);
    let (mut first_buffer, mut second_buffer) = if size > BUF_SIZE {
        // in case the byte count is larger than the amount of memory, only allocate to the size of
        // the data read (in case the file size is much smaller than the byte count, which would
        // mean nothing should be printed rather than the program aborting)
        (vec![], vec![])
    } else {
        (Vec::with_capacity(size), Vec::with_capacity(size))
    };
    let mut prev_buffer = &mut first_buffer;
    let mut cur_buffer = &mut second_buffer;

    loop {
        let n = (&mut input).take(size as u64).read_to_end(cur_buffer)?;
        if n == size {
            output.write_all(prev_buffer)?;
            mem::swap(&mut prev_buffer, &mut cur_buffer);
            cur_buffer.clear();
        } else {
            break;
        }
    }
    if cur_buffer.len() == 0 {
        if bytes < prev_buffer.len() {
            output.write_all(&prev_buffer[..prev_buffer.len() - bytes])?
        }
    } else {
        if bytes < cur_buffer.len() {
            output.write_all(prev_buffer)?;
            output.write_all(&cur_buffer[..cur_buffer.len() - bytes])?;
        } else {
            let bytes = bytes - cur_buffer.len();
            let buf_len = prev_buffer.len();
            output.write_all(&prev_buffer[..buf_len - bytes.min(buf_len)])?;
        }
    }

    Ok(())
}

// returns the number and whether it is positive
#[allow(unused_parens)]
fn parse_num(s: &str) -> Option<(usize, bool)> {
    let s = s.trim();
    let positive = (s.chars().next()? != '-');
    let numstr = if positive {
        s
    } else {
        s.trim_left_matches('-')
    };
    let num = util::parse_num_with_suffix(numstr)?;
    Some((num, positive))
}

fn is_valid_num(val: &OsStr) -> StdResult<(), OsString> {
    let res = val.to_str().and_then(parse_num);
    if res.is_some() {
        Ok(())
    } else {
        Err(OsString::from(format!("'{}' is not a number or is too large", val.to_string_lossy())))
    }
}
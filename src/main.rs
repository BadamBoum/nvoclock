extern crate clap;
#[macro_use]
extern crate prettytable;
extern crate nvapi_hi as nvapi;
#[macro_use]
extern crate quick_error;
#[macro_use]
extern crate log;
extern crate env_logger;
extern crate result;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate csv;

use std::collections::BTreeMap;
use std::process::exit;
use std::time::Duration;
use std::str::FromStr;
use std::io::{self, Write};
use std::{fs, iter};
use nvapi::{
    Status, Gpu,
    Percentage, Celsius, Kilohertz, KilohertzDelta, Microvolts, VfPoint,
    ClockDomain, PState, CoolerPolicy, CoolerLevel, ClockLockMode,
    allowable_result
};
use clap::{Arg, App, SubCommand, AppSettings};
use result::OptionResultExt;

mod auto;
mod human;

const VERSION: &'static str = env!("CARGO_PKG_VERSION");

quick_error! {
    #[derive(Debug)]
    pub enum Error {
        Nvapi(err: Status) {
            from()
            display("NVAPI error: {}", nvapi::error_message(*err).unwrap_or_else(|_| format!("{:?}", err)))
        }
        Io(err: io::Error) {
            from()
            cause(err)
            display("IO error: {}", err)
        }
        Json(err: serde_json::Error) {
            from()
            cause(err)
            display("JSON error: {}", err)
        }
        ParseInt(err: ::std::num::ParseIntError) {
            from()
            cause(err)
            display("{}", err)
        }
        Str(err: &'static str) {
            from()
            display("{}", err)
        }
        ResetError { setting: ResetSettings, err: Status } {
            from(s: (ResetSettings, Status)) -> {
                setting: s.0,
                err: s.1
            }
            display("Reset {:?} failed: {}", setting, Error::from(err))
        }
    }
}

impl<'a> From<&'a Status> for Error {
    fn from(s: &'a Status) -> Self {
        s.clone().into()
    }
}

fn main() {
    match main_result() {
        Ok(code) => exit(code),
        Err(e) => {
            let _ = writeln!(io::stderr(), "{}", e);
            exit(1);
        },
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GpuDescriptor {
    pub name: String,
}

#[derive(Debug, Copy, Clone)]
pub enum OutputFormat {
    Human,
    Json,
}

#[derive(Debug, Copy, Clone)]
pub enum ResetSettings {
    VoltageBoost,
    SensorLimits,
    PowerLimits,
    CoolerLevels,
    VfpDeltas,
    VfpLock,
    PStateDeltas,
    Overvolt,
}

trait ConvertEnum: Sized {
    fn from_str(s: &str) -> Result<Self, Error>;
    fn to_str(&self) -> &'static str;
    fn possible_values() -> &'static [&'static str];
    fn possible_values_typed() -> &'static [Self];
}

macro_rules! enum_from_str {
    (
        $conv:ident => {
        $(
            $item:ident = $str:expr,
        )*
            _ => $err:expr,
        }
    ) => {
        impl ConvertEnum for $conv {
            fn from_str(s: &str) -> Result<Self, Error> {
                match s {
                $(
                    $str => Ok($conv::$item),
                )*
                    _ => Err(($err).into()),
                }
            }

            #[allow(unreachable_patterns)]
            fn to_str(&self) -> &'static str {
                match *self {
                $(
                    $conv::$item => $str,
                )*
                    _ => "unknown",
                }
            }

            fn possible_values() -> &'static [&'static str] {
                &[$(
                    $str,
                )*]
            }

            fn possible_values_typed() -> &'static [Self] {
                &[$(
                    $conv::$item,
                )*]
            }
        }
    };
}

enum_from_str! {
    OutputFormat => {
        Human = "human",
        Json = "json",
        _ => "unknown output format",
    }
}

enum_from_str! {
    ResetSettings => {
        VoltageBoost = "voltage-boost",
        SensorLimits = "thermal",
        PowerLimits = "power",
        CoolerLevels = "cooler",
        VfpDeltas = "vfp",
        VfpLock = "lock",
        PStateDeltas = "pstate",
        Overvolt = "overvolt",
        _ => "unknown setting",
    }
}

enum_from_str! {
    PState => {
        P0 = "P0",
        P1 = "P1",
        P2 = "P2",
        P3 = "P3",
        P4 = "P4",
        P5 = "P5",
        P6 = "P6",
        P7 = "P7",
        P8 = "P8",
        P9 = "P9",
        P10 = "P10",
        P11 = "P11",
        P12 = "P12",
        P13 = "P13",
        P14 = "P14",
        P15 = "P15",
        _ => "unknown pstate",
    }
}

enum_from_str! {
    ClockDomain => {
        Graphics = "graphics",
        Memory = "memory",
        Processor = "processor",
        Video = "video",
        _ => "unknown clock type",
    }
}

enum_from_str! {
    CoolerPolicy => {
        None = "default",
        Manual = "manual",
        Performance = "perf",
        TemperatureDiscrete = "discrete",
        TemperatureContinuous = "continuous",
        Hybrid = "hybrid",
        Silent = "silent",
        Unknown32 = "32",
        _ => "unknown cooler policy",
    }
}

fn is_std(str: &str) -> bool {
    str == "-"
}

fn export_vfp<W: Write, I: Iterator<Item=VfPoint>>(write: W, points: I, delimiter: u8) -> io::Result<()> {
    let mut w = csv::WriterBuilder::new().delimiter(delimiter).from_writer(write);

    Ok(for point in points {
        w.serialize(point)?;
    })
}

const POSSIBLE_BOOL_OFF: &'static str = "off";
const POSSIBLE_BOOL_ON: &'static str = "on";
const POSSIBLE_BOOL: &'static [&'static str] = &[POSSIBLE_BOOL_OFF, POSSIBLE_BOOL_ON];

fn parse_bool_match(matches: &clap::ArgMatches, arg: &'static str) -> bool {
    // Weird interaction with defaults and empty values
    let occ = matches.occurrences_of(arg);
    let values = matches.values_of(arg).map(|v| v.count()).unwrap_or(0);
    let value = matches.value_of(arg);

    let v = match (occ, values, value) {
        (1, 1, Some(..)) => POSSIBLE_BOOL_ON,
        (0, 1, Some(v)) => v,
        (1, 2, Some(v)) => v,
        _ => unreachable!(),
    };

    match v {
        POSSIBLE_BOOL_OFF => false,
        POSSIBLE_BOOL_ON => true,
        _ => unreachable!(),
    }
}

fn main_result() -> Result<i32, Error> {
    if let Err(e) = env_logger::init() {
        let _ = writeln!(io::stderr(), "Failed to initialize env_logger: {}", e);
    }

    let app = App::new("newclock")
        .version(VERSION)
        .author("arcnmx")
        .about("NVIDIA overclocking")
        .arg(Arg::with_name("gpu")
            .short("g")
            .long("gpu")
            .value_name("GPU")
            .takes_value(true)
            .multiple(true)
            .default_value("0")
            .help("GPU index")
        ).arg(Arg::with_name("oformat")
            .short("O")
            .long("output-format")
            .value_name("OFORMAT")
            .takes_value(true)
            .possible_values(OutputFormat::possible_values())
            .default_value(OutputFormat::Human.to_str())
            .help("Data output format")
        ).subcommand(SubCommand::with_name("list")
            .about("List detected GPUs")
        ).subcommand(SubCommand::with_name("info")
            .about("Information about the model and capabilities of the GPU")
        ).subcommand(SubCommand::with_name("status")
            .about("Show current GPU usage, sensor, and clock information")
            .arg(Arg::with_name("all")
                .short("a")
                .long("all")
                .help("Show all available info")
            ).arg(Arg::with_name("clocks")
                .short("c")
                .long("clocks")
                .takes_value(true)
                .possible_values(POSSIBLE_BOOL)
                .default_value(POSSIBLE_BOOL_ON)
                .help("Show clock frequency info")
            ).arg(Arg::with_name("coolers")
                .short("C")
                .long("coolers")
                .takes_value(true)
                .possible_values(POSSIBLE_BOOL)
                .default_value(POSSIBLE_BOOL_OFF)
                .default_value_if("all", None, POSSIBLE_BOOL_ON)
                .help("Show cooler info")
            ).arg(Arg::with_name("sensors")
                .short("s")
                .long("sensors")
                .takes_value(true)
                .possible_values(POSSIBLE_BOOL)
                .default_value(POSSIBLE_BOOL_OFF)
                .default_value_if("all", None, POSSIBLE_BOOL_ON)
                .help("Show thermal sensors")
            ).arg(Arg::with_name("vfp")
                .short("v")
                .long("vfp")
                .takes_value(true)
                .possible_values(POSSIBLE_BOOL)
                .default_value(POSSIBLE_BOOL_OFF)
                .default_value_if("all", None, POSSIBLE_BOOL_ON)
                .help("Show voltage-frequency chart")
            ).arg(Arg::with_name("pstates")
                .short("P")
                .long("pstates")
                .takes_value(true)
                .possible_values(POSSIBLE_BOOL)
                .default_value(POSSIBLE_BOOL_OFF)
                .default_value_if("all", None, POSSIBLE_BOOL_ON)
                .help("Show power state configurations")
            )
        ).subcommand(SubCommand::with_name("get")
            .about("Show GPU overclock settings")
        ).subcommand(SubCommand::with_name("reset")
            .about("Restore all overclocking settings")
            .arg(Arg::with_name("setting")
                .value_name("SETTING")
                .takes_value(true)
                .multiple(true)
                .possible_values(ResetSettings::possible_values())
                .help("Reset only the specified setting(s)")
            )
        ).subcommand(SubCommand::with_name("set")
            .about("GPU overclocking")
            .arg(Arg::with_name("vboost")
                .short("V")
                .long("voltage-boost")
                .value_name("VBOOST")
                .takes_value(true)
                .help("Voltage Boost %")
            ).arg(Arg::with_name("tlimit")
                .short("T")
                .long("thermal-limit")
                .value_name("TEMPLIMIT")
                .takes_value(true)
                .multiple(true)
                .help("Thermal limit (C)")
            ).arg(Arg::with_name("plimit")
                .short("P")
                .long("power-limit")
                .value_name("POWERLIMIT")
                .takes_value(true)
                .multiple(true)
                .help("Power limit %")
            ).subcommand(SubCommand::with_name("pstate")
                .about("Simple offset overclocking")
                .arg(Arg::with_name("pstate")
                    .short("p")
                    .long("pstate")
                    .value_name("PSTATE")
                    .takes_value(true)
                    .possible_values(PState::possible_values())
                    .default_value(PState::P0.to_str())
                    .help("PState number")
                ).arg(Arg::with_name("clock")
                    .short("c")
                    .long("clock")
                    .value_name("CLOCK")
                    .takes_value(true)
                    .possible_values(ClockDomain::possible_values())
                    .default_value(ClockDomain::Graphics.to_str())
                    .help("Clock type")
                ).arg(Arg::with_name("delta")
                    .value_name("DELTA")
                    .takes_value(true)
                    .allow_hyphen_values(true)
                    .required(true)
                    .help("Clock delta (MHz)")
                )
            ).subcommand(SubCommand::with_name("cooler")
                .about("Fan and cooler controls")
                .arg(Arg::with_name("policy")
                    .value_name("MODE")
                    .takes_value(true)
                    .required(true)
                    .possible_values(CoolerPolicy::possible_values())
                    .help("Cooler policy")
                ).arg(Arg::with_name("level")
                    .value_name("LEVEL")
                    .takes_value(true)
                    .required(true)
                    .help("Cooler level %")
                )
            ).subcommand(SubCommand::with_name("vfp")
                .about("GPU Boost 3.0 voltage-frequency curve")
                .subcommand(SubCommand::with_name("export")
                    .about("Export current curve as CSV")
                    .arg(Arg::with_name("tabs")
                        .short("t")
                        .long("tabs")
                        .help("Separate columns using tabs")
                    ).arg(Arg::with_name("output")
                        .value_name("OUTPUT")
                        .takes_value(true)
                        .default_value("-")
                        .help("Output file path")
                    )
                ).subcommand(SubCommand::with_name("import")
                    .about("Import a modified curve from CSV")
                    .arg(Arg::with_name("tabs")
                        .short("t")
                        .long("tabs")
                        .help("Separate columns using tabs")
                    ).arg(Arg::with_name("input")
                        .value_name("INPUT")
                        .takes_value(true)
                        .default_value("-")
                        .help("Input file path")
                    )
                ).subcommand(SubCommand::with_name("lock")
                    .about("Lock the clock to a specific point on the curve")
                    .arg(Arg::with_name("point")
                        .value_name("POINT")
                        .takes_value(true)
                        .required(true)
                        .help("Point index to lock at")
                    ).arg(Arg::with_name("voltage")
                        .short("v")
                        .long("voltage")
                        .help("Interpret point as voltage instead of index")
                    )
                ).subcommand(SubCommand::with_name("unlock")
                    .about("Remove any existing locks")
                ).subcommand(SubCommand::with_name("auto")
                    .about("Run a series of automated tests to determine optimal clocks")
                    .arg(Arg::with_name("fan")
                        .long("fan-override")
                        .help("Prevent fan from running full throttle (not recommended, high temperatures skew results)")
                    ).arg(Arg::with_name("step")
                        .value_name("STEP")
                        .short("S")
                        .long("step")
                        .takes_value(true)
                        .default_value("16")
                        .help("Testing step resolution (MHz)")
                    ).arg(Arg::with_name("max")
                        .value_name("MAX")
                        .short("M")
                        .long("max")
                        .takes_value(true)
                        .default_value("2200")
                        .help("Testing max frequency (MHz)")
                    ).arg(Arg::with_name("start")
                        .value_name("START")
                        .short("s")
                        .long("start")
                        .takes_value(true)
                        .default_value("0")
                        .help("Point index to start at")
                    ).arg(Arg::with_name("end")
                        .value_name("END")
                        .short("e")
                        .long("end")
                        .takes_value(true)
                        .help("Point index to end at")
                    ).arg(Arg::with_name("test")
                        .value_name("TEST")
                        .short("t")
                        .long("test")
                        .takes_value(true)
                        .help("Testing binary to use (see `help auto test`)")
                    ).subcommand(SubCommand::with_name("test")
                        .about("Runs a single test cycle, monitoring the GPU and waiting for a stress test to run. Do not use this command directly.")
                        .arg(Arg::with_name("voltage")
                            .value_name("VOLTAGE")
                            .takes_value(true)
                            .required(true)
                            .help("Voltage of point to test")
                        ).arg(Arg::with_name("clock")
                            .value_name("CLOCK")
                            .takes_value(true)
                            .required(true)
                            .help("Clock frequency of point to test")
                        )
                    )
                )
            ).subcommand(SubCommand::with_name("overvolt")
                .arg(Arg::with_name("voltage")
                    .value_name("VOLTAGE")
                    .multiple(true)
                    .takes_value(true)
                    .help("Voltage")
                )
            )
        ).setting(AppSettings::SubcommandRequiredElseHelp);

    let matches = app.get_matches();

    let mut exit_code = 0;

    nvapi::initialize()?;

    let driver_version = nvapi::driver_version()?;
    info!("Driver version: {} ({})", driver_version.1, driver_version.0);
    info!("Interface version: {}", nvapi::interface_version()?);

    let gpu = matches.values_of("gpu");

    fn single_gpu<'a>(gpus: &[&'a Gpu]) -> Result<&'a Gpu, Error> {
        let mut gpus = gpus.into_iter();
        gpus.next().ok_or_else(|| Error::from("no GPU selected"))
            .and_then(|g| match gpus.next() {
                None => Ok(*g),
                Some(..) => Err(Error::from("multiple GPUs selected")),
            })
    }

    fn select_gpus<'a>(gpus: &'a [Gpu], gpu: Option<clap::Values>) -> Result<Vec<&'a Gpu>, Error> {
        let v = match gpu {
            Some(gpu) => {
                let gpu = gpu.map(usize::from_str).collect::<Result<Vec<_>, _>>()?;

                gpus.iter().enumerate().filter_map(|(i, g)| {
                    for &gpu in &gpu {
                        if i == gpu {
                            return Some(g)
                        }
                    }

                    None
                }).collect::<Vec<_>>()
            },
            None => gpus.into_iter().collect(),
        };

        if v.is_empty() {
            Err(Error::from(Status::NvidiaDeviceNotFound))
        } else {
            Ok(v)
        }
    }

    let oformat = matches.value_of("oformat").map(OutputFormat::from_str).unwrap()?;

    match matches.subcommand() {
        ("list", Some(..)) => {
            let gpus = Gpu::enumerate()?
                .into_iter()
                .map(|gpu| Ok::<_, Status>(GpuDescriptor {
                    name: gpu.inner().full_name()?,
                })).collect::<Result<Vec<_>, _>>()?;

            match oformat {
                OutputFormat::Human => for (i, gpu) in gpus.into_iter().enumerate() {
                    println!("GPU #{}: {}", i, gpu.name);
                },
                OutputFormat::Json => {
                    serde_json::to_writer_pretty(io::stdout(), &gpus)?
                },
            }
        },
        ("info", Some(matches)) => {
            let gpus = Gpu::enumerate()?;
            let gpus = select_gpus(&gpus, gpu)?;

            match oformat {
                OutputFormat::Human => {

                    for gpu in gpus {
                        let info = gpu.info()?;
                        human::print_info(&info);
                        println!();
                    }
                },
                OutputFormat::Json => {
                    serde_json::to_writer_pretty(
                        io::stdout(),
                        &gpus.into_iter().map(|gpu| gpu.info()).collect::<Result<Vec<_>, _>>()?
                    )?;
                },
            }
        },
        ("status", Some(matches)) => {
            let gpus = Gpu::enumerate()?;
            let gpus = select_gpus(&gpus, gpu)?;

            match oformat {
                OutputFormat::Human => {
                    let show_clocks = parse_bool_match(&matches, "clocks");
                    let show_coolers = parse_bool_match(&matches, "coolers");
                    let show_sensors = parse_bool_match(&matches, "sensors");
                    let show_vfp = parse_bool_match(&matches, "vfp");
                    let show_pstates = parse_bool_match(&matches, "pstates");

                    for gpu in gpus {
                        let status = gpu.status()?;
                        human::print_status(&status);

                        let set = gpu.settings()?;
                        human::print_settings(&set);

                        println!();

                        let info = gpu.info()?;

                        if show_clocks {
                            human::print_clocks(&info.base_clocks, &info.boost_clocks, &status.clocks, &status.utilization);
                        }

                        if show_sensors {
                            human::print_sensors(status.sensors.iter()
                                .zip(info.sensor_limits.iter().zip(set.sensor_limits.iter().cloned())
                                    .map(Some).chain(iter::repeat(None))
                                ).map(|(&(ref desc, temp), limit)| (desc, limit, temp))
                            );
                        }

                        if show_coolers {
                            human::print_coolers(
                                status.coolers.iter().map(|&(ref desc, ref cooler)| (desc, cooler)),
                                status.tachometer.ok()
                            );
                        }

                        if show_vfp {
                            let vfp = status.vfp.as_ref()?;
                            let vfp_deltas = set.vfp.as_ref()?;
                            let lock = set.vfp_locks.iter().map(|(_, e)| e)
                                .filter(|&e| e.mode == ClockLockMode::Manual).map(|e| e.voltage).max();
                            human::print_vfp(vfp.graphics.iter().zip(vfp_deltas.graphics.iter())
                                .map(|((i0, p), (i1, d))| {
                                    assert_eq!(i0, i1);
                                    (*i0, VfPoint::new(p.clone(), d.clone()))
                                }),
                                lock, status.voltage.ok()
                            );
                        }

                        if show_pstates {
                            let set = &set;
                            human::print_pstates(info.pstate_limits.iter()
                                .flat_map(|(&p, e)| e.iter().map(move |(&c, e)|
                                    (p, c, e,
                                        set.pstate_deltas.get(&p).and_then(|p| p.get(&c).cloned())
                                    )
                                )),
                                Some(status.pstate)
                            );
                        }

                        println!();
                    }
                },
                OutputFormat::Json => {
                    serde_json::to_writer_pretty(
                        io::stdout(),
                        &gpus.into_iter().map(|gpu| gpu.status()).collect::<Result<Vec<_>, _>>()?
                    )?;
                },
            }
        },
        ("get", Some(..)) => {
            let gpus = Gpu::enumerate()?;
            let gpus = select_gpus(&gpus, gpu)?;

            match oformat {
                OutputFormat::Human => {
                    for gpu in gpus {
                        let set = gpu.settings()?;
                        human::print_settings(&set);
                    }
                },
                OutputFormat::Json => {
                    serde_json::to_writer_pretty(
                        io::stdout(),
                        &gpus.into_iter().map(|gpu| gpu.settings()).collect::<Result<Vec<_>, _>>()?
                    )?;
                },
            }
        },
        ("reset", Some(matches)) => {
            let gpus = Gpu::enumerate()?;
            let gpus = select_gpus(&gpus, gpu)?;

            let (settings, explicit) = if let Some(reset) = matches.values_of("reset") {
                (reset.map(ResetSettings::from_str).collect::<Result<_, _>>()?, true)
            } else {
                (ResetSettings::possible_values_typed().iter().cloned().collect::<Vec<_>>(), false)
            };

            fn warn_result(r: nvapi::Result<()>, setting: ResetSettings, explicit: bool) -> Result<(), Error> {
                match (allowable_result(r).map_err(|e| (setting, e))?, explicit) {
                    (Err(e), true) => Err((setting, e).into()),
                    _ => Ok(()),
                }
            }

            for gpu in gpus {
                let info = gpu.info()?;

                for &setting in &settings {
                    match setting {
                        ResetSettings::VoltageBoost => warn_result(
                            gpu.set_voltage_boost(Percentage(0)),
                            setting, explicit
                        )?,
                        ResetSettings::SensorLimits => warn_result(
                            gpu.set_sensor_limits(info.sensor_limits.iter().map(|info| info.default)),
                            setting, explicit
                        )?,
                        ResetSettings::PowerLimits => warn_result(
                            gpu.set_power_limits(info.power_limits.iter().map(|info| info.default)),
                            setting, explicit
                        )?,
                        ResetSettings::CoolerLevels => warn_result(
                            gpu.reset_cooler_levels(),
                            setting, explicit
                        )?,
                        ResetSettings::VfpDeltas => warn_result(
                            gpu.reset_vfp(), // not really necessary if we're also doing pstate reset?
                            setting, explicit
                        )?,
                        ResetSettings::VfpLock => warn_result(
                            gpu.reset_vfp_lock(),
                            setting, explicit
                        )?,
                        ResetSettings::PStateDeltas => {
                            let pstates = info.pstate_limits.iter().flat_map(|(&pstate, l)|
                                l.iter()
                                    .filter(|&(_, ref info)| info.frequency_delta.is_some())
                                    .map(move |(&clock, _)| (pstate, clock))
                            );
                            warn_result(
                                gpu.inner().set_pstates(pstates.map(|(pstate, clock)| (pstate, clock, KilohertzDelta(0)))),
                                setting, explicit
                            )?
                        },
                        ResetSettings::Overvolt => warn_result(
                            // TODO: reset overvolt
                            Err(Status::NoImplementation),
                            setting, explicit
                        )?,
                    }
                }
            }
        },
        ("set", Some(matches)) => {
            let gpus = Gpu::enumerate()?;
            let gpus = select_gpus(&gpus, gpu)?;

            for gpu in &gpus {
                if let Some(vboost) = matches.value_of("vboost").map(u32::from_str).invert()? {
                    gpu.set_voltage_boost(Percentage(vboost))?
                }

                if let Some(plimit) = matches.values_of("plimit") {
                    let plimit = plimit.map(u32::from_str).map(|v| v.map(|v| Percentage(v))).collect::<Result<Vec<_>, _>>()?;
                    gpu.set_power_limits(plimit.into_iter())?
                }

                if let Some(tlimit) = matches.values_of("tlimit") {
                    let tlimit = tlimit.map(i32::from_str).map(|v| v.map(|v| Celsius(v))).collect::<Result<Vec<_>, _>>()?;
                    gpu.set_sensor_limits(tlimit.into_iter())?
                }
            }

            match matches.subcommand() {
                ("pstate", Some(matches)) => {
                    for gpu in &gpus {
                        let pstate = matches.value_of("pstate").map(PState::from_str).unwrap()?;
                        let clock = matches.value_of("clock").map(ClockDomain::from_str).unwrap()?;
                        let delta = matches.value_of("delta").map(i32::from_str).unwrap()?;

                        gpu.inner().set_pstates([(pstate, clock, KilohertzDelta(delta))].iter().cloned())?
                    }
                },
                ("cooler", Some(matches)) => {
                    for gpu in &gpus {
                        let mode = matches.value_of("policy").map(CoolerPolicy::from_str).unwrap()?;
                        let level = matches.value_of("level").map(u32::from_str).unwrap()?;

                        gpu.set_cooler_levels(vec![CoolerLevel {
                            policy: mode,
                            level: Percentage(level),
                        }].into_iter())?
                    }
                },
                ("vfp", Some(matches)) => {
                    match matches.subcommand() {
                        ("export", Some(matches)) => {
                            let gpu = single_gpu(&gpus)?;
                            let delimiter = if matches.is_present("tabs") { b'\t' } else { b',' };
                            let output = matches.value_of("output").unwrap();

                            let status = gpu.status()?;
                            let settings = gpu.settings()?;

                            let points = status.vfp?.graphics.into_iter().zip(settings.vfp?.graphics.into_iter())
                                .map(|((i0, point), (i1, delta))| {
                                    assert_eq!(i0, i1);
                                    VfPoint::new(point, delta)
                                });

                            if is_std(output) {
                                export_vfp(io::stdout(), points, delimiter)
                            } else {
                                export_vfp(fs::File::create(output)?, points, delimiter)
                            }?
                        },
                        ("import", Some(matches)) => {
                            for gpu in &gpus {
                                let delimiter = if matches.is_present("tabs") { b'\t' } else { b',' };
                                let input = matches.value_of("input").unwrap();

                                let status = gpu.status()?;
                                let vfp = status.vfp?.graphics;

                                fn import<R: io::Read>(read: R, delimiter: u8) -> Result<Vec<VfPoint>, csv::Error> {
                                    let mut csv = csv::ReaderBuilder::new().delimiter(delimiter).from_reader(read);
                                    let de = csv.deserialize();

                                    de.collect()
                                }

                                let input = if is_std(input) {
                                    import(io::stdin(), delimiter)
                                } else {
                                    import(fs::File::open(input)?, delimiter)
                                }.map_err(io::Error::from)?;

                                gpu.inner().set_vfp_table(
                                    [0, 0, 0, 0],
                                    input.into_iter().filter_map(|point|
                                        vfp.iter()
                                            .find(|&(_, ref v)| v.voltage == point.voltage)
                                            .map(|(&i, _)| (i, point.delta.into()))
                                    ),
                                    ::std::iter::empty(),
                                )?;
                            }
                        },
                        ("lock", Some(matches)) => {
                            for gpu in &gpus {
                                let point = matches.value_of("point").map(u32::from_str).unwrap()?;
                                let v = if matches.is_present("voltage") {
                                    Microvolts(point)
                                } else {
                                    gpu.status()?.vfp?.graphics.get(&(point as usize))
                                        .ok_or(Error::Str("invalid point index"))?
                                        .voltage
                                };

                                gpu.set_vfp_lock(v)?;
                            }
                        },
                        ("unlock", Some(..)) => {
                            for gpu in &gpus {
                                gpu.reset_vfp_lock()?;
                            }
                        },
                        ("auto", Some(matches)) => {
                            let gpu = single_gpu(&gpus)?;

                            let end = matches.value_of("end").map(usize::from_str).invert()?;
                            let start = matches.value_of("start").map(usize::from_str).unwrap()?;
                            let step = matches.value_of("step").map(i32::from_str).unwrap()?;
                            let max = matches.value_of("max").map(u32::from_str).unwrap()?;

                            let status = gpu.status()?;
                            let vfp = status.vfp?;
                            let settings = gpu.settings()?;
                            let vfp_delta = settings.vfp?;
                            let end = end.unwrap_or(vfp.graphics.iter().map(|(&i, _)| i).max().unwrap());

                            let options = auto::AutoDetectOptions {
                                fan_override: matches.is_present("fan"),
                                step: KilohertzDelta(step * 1000),
                                test: matches.value_of("test").map(|v| v.to_owned()),
                                voltage_wait_delay: Duration::from_secs(2),
                                max_frequency: Kilohertz(max * 1000),
                            };

                            let mut auto = auto::AutoDetect::new(&gpu, options)?;
                            let mut results: BTreeMap<usize, VfPoint> = Default::default();

                            auto.test_prepare()?;

                            for (i, point, delta) in (start..end).rev()
                                .filter_map(|i| vfp.graphics.get(&i).map(|v| (i, v)))
                                .map(|(i, v)| (i, v, vfp_delta.graphics.get(&i).unwrap()))
                            {
                                match auto.test_point(i, point.voltage, point.frequency, *delta) {
                                    Ok(Some((delta, frequency))) => {
                                        results.insert(i, VfPoint {
                                            voltage: point.voltage,
                                            frequency: frequency,
                                            delta: delta,
                                        });

                                        info!("found best point: {:#?}", frequency);
                                    },
                                    Ok(None) => (),
                                    Err(e) => {
                                        let _ = auto.test_cleanup();

                                        let _ = export_vfp(io::stdout(), results.into_iter().map(|(_, v)| v), b',');

                                        return Err(e)
                                    },
                                }
                            }

                            let res = auto.test_cleanup();

                            let io_res = export_vfp(io::stdout(), results.into_iter().map(|(_, v)| v), b',');

                            let _ = res.and_then(|_| io_res.map_err(From::from))?;
                        },
                        _ => unreachable!("unknown command"),
                    }
                },
                ("overvolt", Some(matches)) => {
                    unimplemented!()
                },
                ("", ..) => (),
                _ => unreachable!("unknown command"),
            }
        },
        _ => unreachable!("unknown command"),
    }

    Ok(exit_code)
}
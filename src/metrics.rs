use core_foundation::dictionary::CFDictionaryRef;

use crate::sources::{
  cfio_bytes, cfio_get_residencies, cfio_watts, libc_ram, libc_swap, IOHIDSensors, IOReport,
  SocInfo, SMC,
};

type WithError<T> = Result<T, Box<dyn std::error::Error>>;

// const CPU_FREQ_DICE_SUBG: &str = "CPU Complex Performance States";
const CPU_FREQ_CORE_SUBG: &str = "CPU Core Performance States";
const GPU_FREQ_DICE_SUBG: &str = "GPU Performance States";

// MARK: Structs

#[derive(Debug, Default)]
pub struct TempMetrics {
  pub cpu_temp_avg: f32, // Celsius
  pub gpu_temp_avg: f32, // Celsius
}

#[derive(Debug, Default)]
pub struct MemMetrics {
  pub ram_total: u64,  // bytes
  pub ram_usage: u64,  // bytes
  pub swap_total: u64, // bytes
  pub swap_usage: u64, // bytes
}

#[derive(Debug, Default, Clone)]
pub struct MetricHistogram {
  pub labels: Vec<String>,
  pub values: Vec<u64>,
}

// impl MetricHistogram {
//   pub fn weighted_average_gbps(&self) -> f64 {
//     let mut sum_products = 0.0;
//     let sum_weights: u64 = self.values.iter().sum();

//     for (label, &weight) in self.labels.iter().zip(self.values.iter()) {
//       if let Ok(gbps) = label.trim().replace("GB/s", "").parse::<f64>() {
//         sum_products += gbps * (weight as f64);
//       }
//     }

//     if sum_weights > 0 {
//       sum_products / (sum_weights as f64)
//     } else {
//       0.0
//     }
//   }
// }

#[derive(Debug, Default)]
pub struct Metrics {
  pub temp: TempMetrics,
  pub memory: MemMetrics,
  pub ecpu_usage: (u32, f32),       // freq, percent_from_max
  pub pcpu_usage: (u32, f32),       // freq, percent_from_max
  pub gpu_usage: (u32, f32),        // freq, percent_from_max
  pub cpu_power: f32,               // Watts
  pub gpu_power: f32,               // Watts
  pub ane_power: f32,               // Watts
  pub all_power: f32,               // Watts
  pub sys_power: f32,               // Watts
  pub ane_rd_bw: u64,               // ANE read bandwidth in B/s
  pub ane_wr_bw: u64,               // ANE write bandwidth in B/s
  pub sys_rd_bw: u64,               // system read bandwidth in B/s (excluding ANE)
  pub sys_wr_bw: u64,               // system write bandwidth in B/s (excluding ANE)
  pub ane_bw_hist: MetricHistogram, // GB/s distribution of ANE read+write BW
}

// MARK: Helpers

pub fn zero_div<T: core::ops::Div<Output = T> + Default + PartialEq>(a: T, b: T) -> T {
  let zero: T = Default::default();
  return if b == zero { zero } else { a / b };
}

fn calc_freq(item: CFDictionaryRef, freqs: &Vec<u32>) -> (u32, f32) {
  let items = cfio_get_residencies(item); // (ns, freq)
  let (len1, len2) = (items.len(), freqs.len());
  assert!(len1 > len2, "cacl_freq invalid data: {} vs {}", len1, len2); // todo?

  // IDLE / DOWN for CPU; OFF for GPU; DOWN only on M2?/M3 Max Chips
  let offset = items.iter().position(|x| x.0 != "IDLE" && x.0 != "DOWN" && x.0 != "OFF").unwrap();

  let usage = items.iter().map(|x| x.1 as f64).skip(offset).sum::<f64>();
  let total = items.iter().map(|x| x.1 as f64).sum::<f64>();
  let count = freqs.len();

  let mut avg_freq = 0f64;
  for i in 0..count {
    let percent = zero_div(items[i + offset].1 as _, usage);
    avg_freq += percent * freqs[i] as f64;
  }

  let usage_ratio = zero_div(usage, total);
  let min_freq = freqs.first().unwrap().clone() as f64;
  let max_freq = freqs.last().unwrap().clone() as f64;
  let from_max = (avg_freq.max(min_freq) * usage_ratio) / max_freq;

  (avg_freq as u32, from_max as f32)
}

fn calc_freq_final(items: &Vec<(u32, f32)>, freqs: &Vec<u32>) -> (u32, f32) {
  let avg_freq = zero_div(items.iter().map(|x| x.0 as f32).sum(), items.len() as f32);
  let avg_perc = zero_div(items.iter().map(|x| x.1 as f32).sum(), items.len() as f32);
  let min_freq = freqs.first().unwrap().clone() as f32;

  (avg_freq.max(min_freq) as u32, avg_perc)
}

fn sum_hist(a: &MetricHistogram, b: &MetricHistogram) -> MetricHistogram {
  let mut labels = a.labels.clone();
  let mut values = a.values.clone();

  for i in 0..b.values.len() {
    let idx = labels.iter().position(|x| x == &b.labels[i]);
    match idx {
      Some(idx) => values[idx] += b.values[i],
      None => {
        labels.push(b.labels[i].clone());
        values.push(b.values[i]);
      }
    }
  }

  MetricHistogram { labels, values }
}

fn zero_div_hist(a: &MetricHistogram, b: u64) -> MetricHistogram {
  let values = a.values.iter().map(|x| zero_div(*x, b)).collect();
  MetricHistogram { labels: a.labels.clone(), values }
}

fn init_smc() -> WithError<(SMC, Vec<String>, Vec<String>)> {
  let mut smc = SMC::new()?;

  let mut cpu_sensors = Vec::new();
  let mut gpu_sensors = Vec::new();

  let names = smc.read_all_keys().unwrap_or(vec![]);
  for name in &names {
    let key = match smc.read_key_info(&name) {
      Ok(key) => key,
      Err(_) => continue,
    };

    if key.data_size != 4 || key.data_type != 1718383648 {
      continue;
    }

    let _ = match smc.read_val(&name) {
      Ok(val) => val,
      Err(_) => continue,
    };

    // Unfortunately, it is not known which keys are responsible for what.
    // Basically in the code that can be found publicly "Tp" is used for CPU and "Tg" for GPU.

    match name {
      name if name.starts_with("Tp") => cpu_sensors.push(name.clone()),
      name if name.starts_with("Tg") => gpu_sensors.push(name.clone()),
      _ => (),
    }
  }

  // println!("{} {}", cpu_sensors.len(), gpu_sensors.len());
  Ok((smc, cpu_sensors, gpu_sensors))
}

// MARK: Sampler

pub struct Sampler {
  soc: SocInfo,
  ior: IOReport,
  hid: IOHIDSensors,
  smc: SMC,
  smc_cpu_keys: Vec<String>,
  smc_gpu_keys: Vec<String>,
}

impl Sampler {
  pub fn new() -> WithError<Self> {
    let channels = vec![
      ("Energy Model", None), // cpu/gpu/ane power
      // ("CPU Stats", Some(CPU_FREQ_DICE_SUBG)), // cpu freq by cluster
      ("CPU Stats", Some(CPU_FREQ_CORE_SUBG)), // cpu freq per core
      ("GPU Stats", Some(GPU_FREQ_DICE_SUBG)), // gpu freq
      ("AMC Stats", Some("Perf Counters")),    // mem bw
      ("PMP", Some("AF BW")),                  // "Apple Fabric" (?) mem bw histogram
    ];

    let soc = SocInfo::new()?;
    let ior = IOReport::new(channels)?;
    let hid = IOHIDSensors::new()?;
    let (smc, smc_cpu_keys, smc_gpu_keys) = init_smc()?;

    Ok(Sampler { soc, ior, hid, smc, smc_cpu_keys, smc_gpu_keys })
  }

  fn get_temp_smc(&mut self) -> WithError<TempMetrics> {
    let mut cpu_metrics = Vec::new();
    for sensor in &self.smc_cpu_keys {
      let val = self.smc.read_val(sensor)?;
      let val = f32::from_le_bytes(val.data[0..4].try_into().unwrap());
      cpu_metrics.push(val);
    }

    let mut gpu_metrics = Vec::new();
    for sensor in &self.smc_gpu_keys {
      let val = self.smc.read_val(sensor)?;
      let val = f32::from_le_bytes(val.data[0..4].try_into().unwrap());
      gpu_metrics.push(val);
    }

    let cpu_temp_avg = zero_div(cpu_metrics.iter().sum::<f32>(), cpu_metrics.len() as f32);
    let gpu_temp_avg = zero_div(gpu_metrics.iter().sum::<f32>(), gpu_metrics.len() as f32);

    Ok(TempMetrics { cpu_temp_avg, gpu_temp_avg })
  }

  fn get_temp_hid(&mut self) -> WithError<TempMetrics> {
    let metrics = self.hid.get_metrics();

    let mut cpu_values = Vec::new();
    let mut gpu_values = Vec::new();

    for (name, value) in &metrics {
      if name.starts_with("pACC MTR Temp Sensor") || name.starts_with("eACC MTR Temp Sensor") {
        // println!("{}: {}", name, value);
        cpu_values.push(*value);
        continue;
      }

      if name.starts_with("GPU MTR Temp Sensor") {
        // println!("{}: {}", name, value);
        gpu_values.push(*value);
        continue;
      }
    }

    let cpu_temp_avg = zero_div(cpu_values.iter().sum(), cpu_values.len() as f32);
    let gpu_temp_avg = zero_div(gpu_values.iter().sum(), gpu_values.len() as f32);

    Ok(TempMetrics { cpu_temp_avg, gpu_temp_avg })
  }

  fn get_temp(&mut self) -> WithError<TempMetrics> {
    // HID for M1, SMC for M2/M3
    // UPD: Looks like HID/SMC related to OS version, not to the chip (SMC available from macOS 14)
    match self.smc_cpu_keys.len() > 0 {
      true => self.get_temp_smc(),
      false => self.get_temp_hid(),
    }
  }

  fn get_mem(&mut self) -> WithError<MemMetrics> {
    let (ram_usage, ram_total) = libc_ram()?;
    let (swap_usage, swap_total) = libc_swap()?;
    Ok(MemMetrics { ram_total, ram_usage, swap_total, swap_usage })
  }

  fn get_sys_power(&mut self) -> WithError<f32> {
    let val = self.smc.read_val("PSTR")?;
    let val = f32::from_le_bytes(val.data.clone().try_into().unwrap());
    Ok(val)
  }

  pub fn get_metrics(&mut self, duration: u64) -> WithError<Metrics> {
    let measures: usize = 4;
    let mut results: Vec<Metrics> = Vec::with_capacity(measures);

    // do several samples to smooth metrics
    // see: https://github.com/vladkens/macmon/issues/10
    for (sample, sample_dt) in self.ior.get_samples(duration, measures) {
      let mut ecpu_usages = Vec::new();
      let mut pcpu_usages = Vec::new();
      let mut rs = Metrics::default();

      for x in sample {
        if x.group == "CPU Stats" && x.subgroup == CPU_FREQ_CORE_SUBG {
          if x.channel.contains("ECPU") {
            ecpu_usages.push(calc_freq(x.item, &self.soc.ecpu_freqs));
            continue;
          }

          if x.channel.contains("PCPU") {
            pcpu_usages.push(calc_freq(x.item, &self.soc.pcpu_freqs));
            continue;
          }
        }

        if x.group == "GPU Stats" && x.subgroup == GPU_FREQ_DICE_SUBG {
          match x.channel.as_str() {
            "GPUPH" => rs.gpu_usage = calc_freq(x.item, &self.soc.gpu_freqs[1..].to_vec()),
            _ => {}
          }
        }

        if x.group == "Energy Model" {
          match x.channel.as_str() {
            "CPU Energy" => rs.cpu_power += cfio_watts(x.item, &x.unit, sample_dt)?,
            "GPU Energy" => rs.gpu_power += cfio_watts(x.item, &x.unit, sample_dt)?,
            c if c.starts_with("ANE") => rs.ane_power += cfio_watts(x.item, &x.unit, sample_dt)?,
            _ => {}
          }
        }

        if x.group == "AMC Stats" && x.subgroup == "Perf Counters" {
          match x.channel.as_str() {
            // There are two values for ANE: "ANE0 DCS RD" and "ANE0 RD".
            // Per asitop, DCS = DRAM Command Scheduler. When asitop
            // supported bandwidth, it used the DCS values (for CPU).
            // The DCS value also seems closer to the number I expect for a test
            // ANE model based on the size of the weights and the tokens/second.
            // The non-DCS value is slightly higher.
            s if s.starts_with("ANE") && s.ends_with("DCS RD") => {
              rs.ane_rd_bw += cfio_bytes(x.item); // bytes
            }
            s if s.starts_with("ANE") && s.ends_with("DCS WR") => {
              rs.ane_wr_bw += cfio_bytes(x.item); // bytes
            }
            // There doesn't seem to be a non-DCS overall metric.
            // NOTE: This includes the ANE value, which is subtracted out below.
            "DCS RD" => {
              rs.sys_rd_bw += cfio_bytes(x.item); // bytes
            }
            "DCS WR" => {
              rs.sys_wr_bw += cfio_bytes(x.item); // bytes
            }
            _ => {}
          }
        }

        if x.group == "PMP" && x.subgroup == "AF BW" {
          match x.channel.as_str() {
            s if s.starts_with("ANE") && s.ends_with("RD+WR") => {
              let events = cfio_get_residencies(x.item);
              if rs.ane_bw_hist.values.len() == 0 {
                rs.ane_bw_hist.labels = events.iter().map(|x| x.0.clone()).collect();
                rs.ane_bw_hist.values = vec![0; events.len()];
              }
              assert!(
                rs.ane_bw_hist.values.len() == events.len(),
                "ANE BW length mismatch: {} != {}",
                rs.ane_bw_hist.values.len(),
                events.len()
              );
              for i in 0..events.len() {
                rs.ane_bw_hist.values[i] += events[i].1 as u64;
              }
            }
            _ => {}
          }
        }
      }

      rs.ecpu_usage = calc_freq_final(&ecpu_usages, &self.soc.ecpu_freqs);
      rs.pcpu_usage = calc_freq_final(&pcpu_usages, &self.soc.pcpu_freqs);
      rs.sys_rd_bw =
        zero_div(rs.sys_rd_bw.saturating_sub(rs.ane_rd_bw) as f64, sample_dt as f64 / 1000.0)
          as u64; // bytes/sec
      rs.sys_wr_bw =
        zero_div(rs.sys_wr_bw.saturating_sub(rs.ane_wr_bw) as f64, sample_dt as f64 / 1000.0)
          as u64; // bytes/sec
      rs.ane_rd_bw = zero_div(rs.ane_rd_bw as f64, sample_dt as f64 / 1000.0) as u64; // bytes/sec
      rs.ane_wr_bw = zero_div(rs.ane_wr_bw as f64, sample_dt as f64 / 1000.0) as u64; // bytes/sec

      results.push(rs);
    }

    let mut rs = Metrics::default();
    rs.ecpu_usage.0 = zero_div(results.iter().map(|x| x.ecpu_usage.0).sum(), measures as _);
    rs.ecpu_usage.1 = zero_div(results.iter().map(|x| x.ecpu_usage.1).sum(), measures as _);
    rs.pcpu_usage.0 = zero_div(results.iter().map(|x| x.pcpu_usage.0).sum(), measures as _);
    rs.pcpu_usage.1 = zero_div(results.iter().map(|x| x.pcpu_usage.1).sum(), measures as _);
    rs.gpu_usage.0 = zero_div(results.iter().map(|x| x.gpu_usage.0).sum(), measures as _);
    rs.gpu_usage.1 = zero_div(results.iter().map(|x| x.gpu_usage.1).sum(), measures as _);
    rs.cpu_power = zero_div(results.iter().map(|x| x.cpu_power).sum(), measures as _);
    rs.gpu_power = zero_div(results.iter().map(|x| x.gpu_power).sum(), measures as _);
    rs.ane_power = zero_div(results.iter().map(|x| x.ane_power).sum(), measures as _);
    rs.all_power = rs.cpu_power + rs.gpu_power + rs.ane_power;

    rs.sys_rd_bw = zero_div(results.iter().map(|x| x.sys_rd_bw).sum(), measures as _); // bytes/sec
    rs.sys_wr_bw = zero_div(results.iter().map(|x| x.sys_wr_bw).sum(), measures as _); // bytes/sec
    rs.ane_rd_bw = zero_div(results.iter().map(|x| x.ane_rd_bw).sum(), measures as _); // bytes/sec
    rs.ane_wr_bw = zero_div(results.iter().map(|x| x.ane_wr_bw).sum(), measures as _); // bytes/sec

    rs.ane_bw_hist = results
      .iter()
      .map(|x| x.ane_bw_hist.clone())
      .reduce(|a, b| sum_hist(&a, &b))
      .map(|hist| zero_div_hist(&hist, measures as _))
      .unwrap_or_default();
    // println!("{:?}", rs.ane_bw_hist);

    rs.memory = self.get_mem()?;
    rs.temp = self.get_temp()?;

    rs.sys_power = match self.get_sys_power() {
      Ok(val) => val.max(rs.all_power),
      Err(_) => 0.0,
    };

    Ok(rs)
  }
}

// SPDX-License-Identifier: GPL-2.0
/*
 * rustinfo_smu - 只读 SMU pm_table 读取模块 (AMD Strix Point)
 *
 * 协议与 ryzen_smu / ryzenadj 相同: 经 PCI 00:00.0 config 0xC4/0xC8 的 SMN 桥,
 * 走 RSMU 邮箱六步握手。只发只读命令:
 *   0x02 GetSMUVersion / 0x06 TableVersion / 0x66 GetDramBase / 0x65 TransferTable
 * 不向固件发送任何修改限值/参数的命令。所有交互带超时与返回码校验, 互斥锁串行化。
 */
#include <linux/module.h>
#include <linux/pci.h>
#include <linux/debugfs.h>
#include <linux/delay.h>
#include <linux/mutex.h>
#include <linux/uaccess.h>
#include <linux/mm.h>
#include <linux/io.h>
#include <linux/slab.h>

#define SMN_ADDR_REG 0xC4
#define SMN_DATA_REG 0xC8
#define TABLE_SIZE   0x2000

static struct pci_dev *smn_pdev;
static DEFINE_MUTEX(smu_lock);

/* 探测命中的邮箱 (SMN 地址) */
static u32 mb_cmd, mb_rsp, mb_args;
static u32 smu_ver, tbl_ver;
static u64 dram_base;
static void *mapped;
static u8 *table_buf;
static struct dentry *dbg;

/* 已知邮箱候选 (来自 ryzen_smu 各代号表, RSMU-APU 优先) */
static const struct { u32 cmd, rsp, args; } mb_cands[] = {
	{ 0x3B10A20, 0x3B10A80, 0x3B10A88 }, /* RSMU-APU */
	{ 0x3B10528, 0x3B10578, 0x3B10998 }, /* MP1 IFv13 */
	{ 0x3B10528, 0x3B10564, 0x3B10998 }, /* MP1 IFv12 */
	{ 0x3B10524, 0x3B10570, 0x3B10A40 }, /* RSMU 桌面 */
	{ 0x3B10530, 0x3B1057C, 0x3B109C4 }, /* MP1 IFv11 */
};

static int smn_read(u32 addr, u32 *val)
{
	int err = pci_write_config_dword(smn_pdev, SMN_ADDR_REG, addr);
	if (err)
		return err;
	return pci_read_config_dword(smn_pdev, SMN_DATA_REG, val);
}

static int smn_write(u32 addr, u32 val)
{
	int err = pci_write_config_dword(smn_pdev, SMN_ADDR_REG, addr);
	if (err)
		return err;
	return pci_write_config_dword(smn_pdev, SMN_DATA_REG, val);
}

/* 六步握手; 返回 0 成功, 否则负 errno。tmp 非 0x01 视为固件拒绝。 */
static int smu_cmd(u32 op, const u32 in[6], u32 out[6])
{
	int retries = 100000, i, err;
	u32 tmp;

	do {
		err = smn_read(mb_rsp, &tmp);
		if (err)
			return err;
		if (tmp)
			break;
		if ((retries % 1000) == 0)
			msleep(1);
	} while (--retries);
	if (!tmp)
		return -ETIMEDOUT;

	err = smn_write(mb_rsp, 0);
	if (err)
		return err;
	for (i = 0; i < 6; i++) {
		err = smn_write(mb_args + 4 * i, in[i]);
		if (err)
			return err;
	}
	err = smn_write(mb_cmd, op);
	if (err)
		return err;

	retries = 200000;
	do {
		err = smn_read(mb_rsp, &tmp);
		if (err)
			return err;
		if (tmp)
			break;
		if ((retries % 1000) == 0)
			msleep(1);
	} while (--retries);
	if (!tmp)
		return -ETIMEDOUT;
	if (tmp != 0x01) {
		pr_warn("rustinfo_smu: cmd 0x%x rejected by SMU: 0x%x\n", op, tmp);
		return -EPROTO;
	}
	for (i = 0; i < 6; i++) {
		err = smn_read(mb_args + 4 * i, &out[i]);
		if (err)
			return err;
	}
	return 0;
}

static int table_refresh(void)
{
	static const u32 zero[6];
	u32 out[6];
	int ret;

	ret = smu_cmd(0x65, zero, out); /* TransferTableSmu2Dram */
	if (ret)
		return ret;
	if (!mapped)
		return -ENODEV;
	memcpy(table_buf, mapped, TABLE_SIZE);
	return 0;
}

static ssize_t table_read(struct file *filp, char __user *ubuf, size_t cnt,
			  loff_t *ppos)
{
	ssize_t ret;

	mutex_lock(&smu_lock);
	if (*ppos == 0) {
		ret = table_refresh();
		if (ret) {
			mutex_unlock(&smu_lock);
			return ret;
		}
	}
	ret = simple_read_from_buffer(ubuf, cnt, ppos, table_buf, TABLE_SIZE);
	mutex_unlock(&smu_lock);
	return ret;
}

static ssize_t info_read(struct file *filp, char __user *ubuf, size_t cnt,
			 loff_t *ppos)
{
	char buf[192];
	int n;

	n = scnprintf(buf, sizeof(buf),
		      "smu_version=0x%08X\ntable_version=0x%06X\ndram_base=0x%llX\n"
		      "mailbox: cmd=0x%X rsp=0x%X args=0x%X\ntable_bytes=%u\n",
		      smu_ver, tbl_ver, dram_base, mb_cmd, mb_rsp, mb_args,
		      TABLE_SIZE);
	return simple_read_from_buffer(ubuf, cnt, ppos, buf, n);
}

static const struct file_operations table_fops = {
	.owner = THIS_MODULE,
	.read = table_read,
};

static const struct file_operations info_fops = {
	.owner = THIS_MODULE,
	.read = info_read,
};

static int __init rustinfo_smu_init(void)
{
	static const u32 one_one[6] = { 1, 1, 0, 0, 0, 0 };
	static const u32 zero[6];
	u32 out[6];
	int i, ret;

	smn_pdev = pci_get_domain_bus_and_slot(0, 0, PCI_DEVFN(0, 0));
	if (!smn_pdev) {
		pr_err("rustinfo_smu: 未找到 PCI 00:00.0\n");
		return -ENODEV;
	}

	/* 探测邮箱: 只读版本命令, 命中 arg0>1 的即认定 */
	for (i = 0; i < ARRAY_SIZE(mb_cands); i++) {
		mb_cmd = mb_cands[i].cmd;
		mb_rsp = mb_cands[i].rsp;
		mb_args = mb_cands[i].args;
		ret = smu_cmd(0x02, one_one, out);
		if (!ret && out[0] > 1) {
			smu_ver = out[0];
			break;
		}
	}
	if (i == ARRAY_SIZE(mb_cands)) {
		pr_err("rustinfo_smu: 无邮箱应答, 卸载\n");
		pci_dev_put(smn_pdev);
		return -ENODEV;
	}

	ret = smu_cmd(0x06, zero, out); /* TableVersion */
	if (ret)
		goto err_out;
	tbl_ver = out[0];

	ret = smu_cmd(0x66, one_one, out); /* GetDramBase */
	if (ret)
		goto err_out;
	dram_base = (u64)out[0] | ((u64)out[1] << 32);
	if (!dram_base) {
		ret = -ENODEV;
		goto err_out;
	}

	mapped = memremap(dram_base, TABLE_SIZE, MEMREMAP_WB);
	if (!mapped)
		mapped = memremap(dram_base, TABLE_SIZE, MEMREMAP_WC);
	if (!mapped) {
		pr_err("rustinfo_smu: memremap 0x%llx 失败\n", dram_base);
		ret = -ENOMEM;
		goto err_out;
	}

	table_buf = kmalloc(TABLE_SIZE, GFP_KERNEL);
	if (!table_buf) {
		ret = -ENOMEM;
		goto err_out;
	}

	mutex_lock(&smu_lock);
	ret = table_refresh();
	mutex_unlock(&smu_lock);
	if (ret)
		goto err_out;

	dbg = debugfs_create_dir("rustinfo_smu", NULL);
	debugfs_create_file("table", 0444, dbg, NULL, &table_fops);
	debugfs_create_file("info", 0444, dbg, NULL, &info_fops);

	pr_info("rustinfo_smu: SMU 0x%08X, 表版本 0x%06X, 基址 0x%llX, debugfs=/sys/kernel/debug/rustinfo_smu\n",
		smu_ver, tbl_ver, dram_base);
	return 0;

err_out:
	if (mapped)
		memunmap(mapped);
	kfree(table_buf);
	pci_dev_put(smn_pdev);
	return ret;
}

static void __exit rustinfo_smu_exit(void)
{
	debugfs_remove_recursive(dbg);
	if (mapped)
		memunmap(mapped);
	kfree(table_buf);
	pci_dev_put(smn_pdev);
	pr_info("rustinfo_smu: 卸载\n");
}

module_init(rustinfo_smu_init);
module_exit(rustinfo_smu_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("Read-only AMD SMU pm_table reader (rustinfo)");

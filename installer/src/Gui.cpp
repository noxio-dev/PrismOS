/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - Qt Widgets GUI implementation.
 */

#include "Gui.h"
#include "Cli.h"
#include "DiskScan.h"
#include "Installer.h"
#include "Layout.h"
#include "Util.h"
#include "Version.h"

#include <QApplication>
#include <QCheckBox>
#include <QFile>
#include <QFileDialog>
#include <QFrame>
#include <QHBoxLayout>
#include <QHeaderView>
#include <QLabel>
#include <QLineEdit>
#include <QMessageBox>
#include <QMouseEvent>
#include <QPainter>
#include <QPlainTextEdit>
#include <QProgressBar>
#include <QPushButton>
#include <QSpinBox>
#include <QStackedWidget>
#include <QTableWidget>
#include <QThread>
#include <QTimer>
#include <QToolTip>
#include <QVBoxLayout>
#include <QWidget>

#include <cmath>
#include <functional>

#include <unistd.h>

using prism::humanSize;
using prism::InstallOptions;
using prism::InstallSummary;
using prism::PartitionPlan;
using prism::ProgressHooks;
using prism::SourceInfo;
using prism::TargetInfo;

using prism::MIB;
using prism::SECTOR;
using prism::ESP_MIN_MIB;
using prism::ESP_MAX_MIB;
using prism::ROOT_MAX_MIB;

/* ================================================================== */
/* Installation worker (separate thread).                              */
/* ================================================================== */

class InstallWorker : public QObject {
    Q_OBJECT
public:
    explicit InstallWorker(const InstallOptions& o) : opts_(o) {}

signals:
    void info(const QString& msg);
    void detail(const QString& msg);
    void progress(int percent, const QString& step);
    void finished();
    void failed(const QString& error);

public slots:
    void run()
    {
        prism::Installer installer(opts_);
        ProgressHooks hooks;
        hooks.info = [this](const std::string& m) {
            emit info(QString::fromStdString(m));
        };
        hooks.detail = [this](const std::string& m) {
            emit detail(QString::fromStdString(m));
        };
        hooks.progress = [this](int p, const std::string& s) {
            emit progress(p, QString::fromStdString(s));
        };
        try {
            installer.run(hooks);
            emit finished();
        } catch (const std::exception& ex) {
            emit failed(QString::fromStdString(ex.what()));
        }
    }

private:
    InstallOptions opts_;
};

/* ================================================================== */
/* Side panel: logo + steps.                                           */
/* ================================================================== */

class StepsPanel : public QWidget {
public:
    explicit StepsPanel(QWidget* parent = nullptr) : QWidget(parent)
    {
        setFixedWidth(232);
        setStyleSheet(
            "StepsPanel { background: #171E2C; }"
            "QLabel { color: #8B93A7; }");

        auto* lay = new QVBoxLayout(this);
        lay->setContentsMargins(24, 28, 24, 24);
        lay->setSpacing(0);

        /* "Prism" bar (subtle rainbow gradient). */
        auto* prism = new QFrame();
        prism->setFixedHeight(6);
        prism->setStyleSheet(
            "background: qlineargradient(x1:0, y1:0, x2:1, y2:0,"
            " stop:0 #FF5F6D, stop:0.33 #FFC371,"
            " stop:0.5 #6FE7DD, stop:0.75 #5B6CFF, stop:1 #B06AB3);"
            "border-radius: 3px;");
        lay->addWidget(prism);

        auto* title = new QLabel("PrismOS");
        title->setStyleSheet(
            "color: #FFFFFF; font-size: 26px; font-weight: 700;"
            "letter-spacing: 1px; margin-top: 22px;");
        lay->addWidget(title);

        auto* subtitle = new QLabel("installer " PRISM_INSTALLER_VERSION);
        subtitle->setStyleSheet("color: #5F6B85; font-size: 11px;");
        lay->addWidget(subtitle);

        lay->addSpacing(34);

        const char* steps[] = {
            "Welcome", "Artifacts", "Disk",
            "Partitioning", "Summary", "Installation"
        };
        for (const char* s : steps) {
            auto* l = new QLabel(QString::fromUtf8("  ") + s);
            l->setStyleSheet("font-size: 13px; padding: 6px 0;");
            labels_ << l;
            lay->addWidget(l);
        }
        lay->addStretch(1);

        auto* foot = new QLabel(QString::fromUtf8("BIOS + UEFI · ext4\n")
                                + PRISM_INSTALLER_TARGET);
        foot->setStyleSheet("color: #454E63; font-size: 10px;");
        lay->addWidget(foot);

        setStep(0);
    }

    void setStep(int current)
    {
        for (int i = 0; i < labels_.size(); ++i) {
            QString prefix;
            QString style = "font-size: 13px; padding: 6px 0;";
            if (i < current) {
                prefix = QString::fromUtf8("✓ ");
                style += "color: #45D483;";
            } else if (i == current) {
                prefix = QString::fromUtf8("▸ ");
                style += "color: #FFFFFF; font-weight: 600;";
            } else {
                prefix = QString::fromUtf8("  ");
                style += "color: #525C74;";
            }
            labels_[i]->setText(prefix + labels_[i]->text()
                                     .mid(2).trimmed());
            labels_[i]->setStyleSheet(style);
        }
    }

private:
    QList<QLabel*> labels_;
};

/* ================================================================== */
/* Disk layout bar (colored proportional zones).                       */
/* ================================================================== */

class LayoutBar : public QWidget {
public:
    struct Region {
        QString name;
        QString tooltip;
        uint64_t bytes;
        QColor color;
    };

    explicit LayoutBar(QWidget* parent = nullptr) : QWidget(parent)
    {
        setMouseTracking(true);
        setMinimumHeight(92);
        setFixedHeight(110);
    }

    void setRegions(const QList<Region>& r)
    {
        regions_ = r;
        update();
    }

protected:
    void paintEvent(QPaintEvent*) override
    {
        QPainter p(this);
        p.setRenderHint(QPainter::Antialiasing);

        QRectF all = QRectF(rect()).adjusted(4, 10, -4, -32);
        double total = 0;
        for (const auto& r : regions_) total += double(r.bytes);
        if (total <= 0) return;

        double x = all.x();
        for (const auto& r : regions_) {
            double w = all.width() * double(r.bytes) / total;
            if (w < 2) w = 2;
            QRectF box(x, all.y(), w - 3, all.height());
            p.setPen(Qt::NoPen);
            p.setBrush(r.color);
            p.drawRoundedRect(box, 4, 4);

            /* Label inside if the zone is wide enough. */
            if (w > 62) {
                p.setPen(QColor(255, 255, 255, 230));
                QFont f = p.font();
                f.setPointSizeF(8.5);
                f.setBold(true);
                p.setFont(f);
                p.drawText(box.adjusted(6, 4, -4, -4),
                           Qt::AlignTop | Qt::AlignLeft
                           | Qt::TextWordWrap,
                           r.name);
                p.setFont(QFont());
                p.setPen(QColor(255, 255, 255, 200));
                p.drawText(box.adjusted(6, -4, -4, -18),
                           Qt::AlignBottom | Qt::AlignLeft,
                           QString::fromStdString(humanSize(r.bytes)));
            }
            x += w;
        }

        /* Numeric legend below. */
        p.setPen(QColor(90, 90, 100));
        QFont f = p.font();
        f.setPointSizeF(8.5);
        p.setFont(f);
        QString legend;
        for (const auto& r : regions_) {
            if (!legend.isEmpty()) legend += QString::fromUtf8("   ·   ");
            legend += r.name + " " + QString::fromStdString(humanSize(r.bytes));
        }
        p.drawText(QRectF(rect()).adjusted(4, height() - 26, -4, -4),
                   Qt::AlignHCenter | Qt::TextSingleLine, legend);
    }

    void mouseMoveEvent(QMouseEvent* e) override
    {
        QRectF all = QRectF(rect()).adjusted(4, 10, -4, -32);
        double total = 0;
        for (const auto& r : regions_) total += double(r.bytes);
        if (total <= 0) return QWidget::mouseMoveEvent(e);

        double rel = double(e->pos().x() - all.x()) / all.width();
        if (rel < 0 || rel > 1) return QWidget::mouseMoveEvent(e);

        double acc = 0;
        for (const auto& r : regions_) {
            acc += double(r.bytes) / total;
            if (rel <= acc) {
                QToolTip::showText(e->globalPosition().toPoint(),
                                   r.tooltip, this);
                return;
            }
        }
    }

private:
    QList<Region> regions_;
};

/* ================================================================== */
/* Main window: pages + navigation.                                    */
/* ================================================================== */

class InstallWizard : public QWidget {
    Q_OBJECT
public:
    explicit InstallWizard(const prism::cli::Args& a, QWidget* parent = nullptr)
        : QWidget(parent), args_(a)
    {
        opts_ = a.options;
        setWindowTitle(QString::fromUtf8("PrismOS Installer"));
        setMinimumSize(980, 620);
        resize(1020, 640);

        auto* root = new QHBoxLayout(this);
        root->setContentsMargins(0, 0, 0, 0);
        root->setSpacing(0);

        steps_ = new StepsPanel();
        root->addWidget(steps_);

        auto* right = new QWidget();
        right->setStyleSheet(
            "QWidget { background: #F5F6FA; color: #1E2433;"
            "font-size: 13px; }"
            "QLabel#pageTitle { font-size: 22px; font-weight: 700;"
            "color: #10141F; }"
            "QLabel#pageHint { color: #5A6274; font-size: 12px; }"
            "QLabel#err { color: #C4321D; font-size: 12px; }"
            "QLabel#warnmsg { color: #8A5A00; font-size: 12px; }"
            "QLabel#okmsg { color: #1D7A4F; font-size: 12px; }");
        auto* rightLay = new QVBoxLayout(right);
        rightLay->setContentsMargins(32, 26, 32, 20);
        rightLay->setSpacing(10);

        stack_ = new QStackedWidget();
        rightLay->addWidget(stack_, 1);

        auto* nav = new QHBoxLayout();
        nav->addStretch(1);
        btnPrev_ = new QPushButton(QString::fromUtf8("← Back"));
        btnNext_ = new QPushButton(QString::fromUtf8("Next →"));
        btnPrev_->setStyleSheet(
            "QPushButton { padding: 8px 18px; border: 1px solid #C9CEDC;"
            "border-radius: 6px; background: #FFFFFF; color: #39415A; }"
            "QPushButton:hover { background: #EEF0F6; }"
            "QPushButton:disabled { color: #A6ACC0; }");
        btnNext_->setStyleSheet(
            "QPushButton { padding: 8px 22px; border: none;"
            "border-radius: 6px; background: #4653D8; color: white;"
            "font-weight: 600; }"
            "QPushButton:hover { background: #3543C8; }"
            "QPushButton:disabled { background: #B9BEE3; color: #E8EAF5; }");
        nav->addWidget(btnPrev_);
        nav->addSpacing(10);
        nav->addWidget(btnNext_);
        rightLay->addLayout(nav);

        connect(btnPrev_, &QPushButton::clicked, this, [this] { prev(); });
        connect(btnNext_, &QPushButton::clicked, this, [this] { next(); });

        root->addWidget(right, 1);

        buildPages();
        stack_->setCurrentIndex(0);
        steps_->setStep(0);
        updateNav();
    }

private slots:
    /* ---- Log and progress (called from the GUI thread) ---- */
    void onInfo(const QString& m)
    {
        log_->appendPlainText(m);
        stepLabel_->setText(m.section('\n', 0, 0));
    }
    void onDetail(const QString& m) { log_->appendPlainText(m); }
    void onProgress(int p, const QString& s)
    {
        bar_->setValue(p);
        stepLabel_->setText(s);
    }
    void onFinished()
    {
        bar_->setValue(100);
        stepLabel_->setText(QString::fromUtf8("Installation complete"));
        log_->appendPlainText(QString::fromUtf8(
            "- Installation complete. You can close the wizard. -"));
        doneSummary_->setText(doneText_);
        stack_->setCurrentIndex(6);
        steps_->setStep(6);
        btnPrev_->setVisible(false);
        btnNext_->setText(QString::fromUtf8("Finish"));
        btnNext_->setEnabled(true);
        installing_ = false;
        if (selfTestActive_)
            QTimer::singleShot(200, qApp, [] { qApp->exit(0); });
    }
    void onFailed(const QString& e)
    {
        stepLabel_->setText(QString::fromUtf8("Installation failure"));
        log_->appendPlainText(QString::fromUtf8("ERROR: ") + e);
        if (selfTestActive_) {
            qApp->exit(3);
            return;
        }
        QMessageBox::critical(this, QString::fromUtf8("Failure"),
                              QString::fromUtf8("Installation failed:\n\n")
                              + e);
        btnNext_->setEnabled(true);
        btnNext_->setText(QString::fromUtf8("Retry"));
    }

private:
    /* ---------------------------------------------------------------- */
    /* Page construction.                                                */
    /* ---------------------------------------------------------------- */

    QWidget* titledPage(const QString& title, const QString& hint,
                        QLayout* body)
    {
        auto* page = new QWidget();
        auto* lay = new QVBoxLayout(page);
        lay->setContentsMargins(0, 0, 0, 0);
        lay->setSpacing(8);

        auto* t = new QLabel(title);
        t->setObjectName("pageTitle");
        lay->addWidget(t);
        if (!hint.isEmpty()) {
            auto* h = new QLabel(hint);
            h->setObjectName("pageHint");
            h->setWordWrap(true);
            lay->addWidget(h);
        }
        auto* sep = new QFrame();
        sep->setFrameShape(QFrame::HLine);
        sep->setStyleSheet("color: #D9DCE8;");
        lay->addWidget(sep);
        lay->addSpacing(6);
        lay->addLayout(body, 1);
        return page;
    }

    QLabel* makeErr() 
    {
        auto* l = new QLabel("");
        l->setObjectName("err");
        l->setWordWrap(true);
        return l;
    }

    void buildPages()
    {
        /* ---- 1. Welcome -------------------------------------------- */
        {
            auto* body = new QVBoxLayout();
            body->setSpacing(14);
            auto* intro = new QLabel(QString::fromUtf8(
                "This wizard installs PrismOS onto a disk or an image:\n\n"
                "  •  a minimal ESP (FAT32) as the FIRST partition - only "
                "EFI/BOOT/BOOTX64.EFI for UEFI (some firmwares only boot "
                "from the first partition);\n"
                "  •  the bootloader (boot0/boot1 + BIOS loader in "
                "raw sectors);\n"
                "  •  an ext4 root partition holding the kernel, the Lua "
                "scripts, loader.conf and the root filesystem.\n\n"
                "The target disk will be entirely erased."));
            intro->setWordWrap(true);
            body->addWidget(intro);
            body->addStretch(1);

            auto* tip = new QLabel(QString::fromUtf8(
                "Tip: text mode available with \"--cli\"; risk-free "
                "testing on a disk image (Disk page)."));
            tip->setObjectName("pageHint");
            body->addWidget(tip);

            stack_->addWidget(titledPage(QString::fromUtf8("Welcome"),
                                         QString::fromUtf8(
                                             "Installing PrismOS "
                                             "on x86-64 - BIOS and UEFI"),
                                         body));
        }

        /* ---- 2. Artifacts (source) ---------------------------------- */
        {
            auto* body = new QVBoxLayout();
            body->setSpacing(10);

            body->addWidget(new QLabel(QString::fromUtf8(
                "PrismOS build directory (output of "
                "\"make\"):")));

            auto* row = new QHBoxLayout();
            sourceEdit_ = new QLineEdit(
                QString::fromStdString(opts_.sourceDir));
            auto* browse = new QPushButton(QString::fromUtf8("Browse..."));
            browse->setStyleSheet(
                "QPushButton { padding: 6px 14px; border: 1px solid #C9CEDC;"
                "border-radius: 6px; background: #FFFFFF; }");
            row->addWidget(sourceEdit_, 1);
            row->addWidget(browse);
            body->addLayout(row);

            sourceErr_ = makeErr();
            sourceOk_ = new QLabel("");
            sourceOk_->setObjectName("okmsg");
            sourceOk_->setWordWrap(true);
            body->addWidget(sourceErr_);
            body->addWidget(sourceOk_);
            body->addStretch(1);

            connect(browse, &QPushButton::clicked, this, [this] {
                QString d = QFileDialog::getExistingDirectory(
                    this, QString::fromUtf8("Artifacts directory"),
                    sourceEdit_->text());
                if (!d.isEmpty()) sourceEdit_->setText(d);
            });
            connect(sourceEdit_, &QLineEdit::textChanged, this,
                    [this] { applyNavState(validateSource()); });

            stack_->addWidget(titledPage(QString::fromUtf8("Artifacts"),
                                         QString::fromUtf8(
                                             "boot0.bin, boot1.bin, "
                                             "loader.bin, the esp/ tree "
                                             "and the root/ tree - "
                                             "including boot/kernel/kernel"),
                                         body));
        }

        /* ---- 3. Disk ------------------------------------------------ */
        {
            auto* body = new QVBoxLayout();
            body->setSpacing(10);

            body->addWidget(new QLabel(QString::fromUtf8(
                "Target disk (everything will be erased):")));

            if (::geteuid() != 0) {
                auto* rootWarn = new QLabel(QString::fromUtf8(
                    "⚠ Writing to a disk requires administrator "
                    "rights, normally requested automatically at "
                    "startup (desktop dialog). This run is not "
                    "elevated (PRISM_INSTALLER_NO_ELEVATE "
                    "or explicit image target): only image "
                    "files remain installable."));
                rootWarn->setObjectName("warnmsg");
                rootWarn->setWordWrap(true);
                body->addWidget(rootWarn);
            }

            diskTable_ = new QTableWidget();
            diskTable_->setColumnCount(4);
            diskTable_->setHorizontalHeaderLabels(QStringList()
                << QString::fromUtf8("Device")
                << QString::fromUtf8("Model")
                << QString::fromUtf8("Size")
                << QString::fromUtf8("Removable"));
            diskTable_->setSelectionBehavior(
                QAbstractItemView::SelectRows);
            diskTable_->setSelectionMode(
                QAbstractItemView::SingleSelection);
            diskTable_->setEditTriggers(
                QAbstractItemView::NoEditTriggers);
            diskTable_->verticalHeader()->setVisible(false);
            diskTable_->horizontalHeader()->setStretchLastSection(true);
            diskTable_->setStyleSheet(
                "QTableWidget { background: #FFFFFF; border: 1px solid "
                "#D9DCE8; border-radius: 6px; }"
                "QHeaderView::section { padding: 6px; background: #EEF0F6;"
                "border: none; font-weight: 600; }"
                "QTableWidget::item:selected { background: #4653D8;"
                "color: white; }");
            body->addWidget(diskTable_, 2);

            body->addWidget(new QLabel(QString::fromUtf8(
                "... or image file (QEMU test, no real data "
                "at stake):")));
            auto* row = new QHBoxLayout();
            imageEdit_ = new QLineEdit();
            auto* browse = new QPushButton(QString::fromUtf8("Choose..."));
            browse->setStyleSheet(
                "QPushButton { padding: 6px 14px; border: 1px solid #C9CEDC;"
                "border-radius: 6px; background: #FFFFFF; }");
            row->addWidget(imageEdit_, 1);
            row->addWidget(browse);
            body->addLayout(row);

            auto* sizeRow = new QHBoxLayout();
            sizeRow->addWidget(new QLabel(QString::fromUtf8(
                "Size if the file needs to be created:")));
            imageSizeSpin_ = new QSpinBox();
            imageSizeSpin_->setRange(128, 65536);
            imageSizeSpin_->setSuffix(QString::fromUtf8(" MiB"));
            imageSizeSpin_->setValue(1024);
            sizeRow->addWidget(imageSizeSpin_);
            sizeRow->addStretch(1);
            body->addLayout(sizeRow);

            diskErr_ = makeErr();
            diskOk_ = new QLabel("");
            diskOk_->setObjectName("okmsg");
            body->addWidget(diskErr_);
            body->addWidget(diskOk_);
            body->addStretch(1);

            connect(browse, &QPushButton::clicked, this, [this] {
                QString f = QFileDialog::getSaveFileName(
                    this, QString::fromUtf8("Disk image"),
                    imageEdit_->text(),
                    QString::fromUtf8("Disk images (*.img *.raw);;"
                                      "All files (*)"));
                if (!f.isEmpty()) imageEdit_->setText(f);
            });
            connect(diskTable_, &QTableWidget::itemSelectionChanged, this,
                    [this] { applyNavState(validateDisk()); });
            connect(imageEdit_, &QLineEdit::textChanged, this,
                    [this] { applyNavState(validateDisk()); });

            for (const auto& d : prism::scanDisks()) {
                int r = diskTable_->rowCount();
                diskTable_->insertRow(r);
                diskTable_->setItem(r, 0, new QTableWidgetItem(
                    QString::fromStdString(d.dev)));
                diskTable_->setItem(r, 1, new QTableWidgetItem(
                    QString::fromStdString(d.model.empty()
                        ? QString::fromUtf8("-").toStdString()
                        : d.model)));
                diskTable_->setItem(r, 2, new QTableWidgetItem(
                    QString::fromStdString(humanSize(d.bytes()))));
                diskTable_->setItem(r, 3, new QTableWidgetItem(
                    d.removable ? QString::fromUtf8("yes") : ""));
            }

            stack_->addWidget(titledPage(QString::fromUtf8("Target disk"),
                                         QString::fromUtf8(
                                             "Select a disk from "
                                             "the list, or an image"),
                                         body));
        }

        /* ---- 4. Partitioning ----------------------------------------- */
        {
            auto* body = new QVBoxLayout();
            body->setSpacing(10);

            auto* grid = new QHBoxLayout();
            grid->setSpacing(18);

            auto* col1 = new QVBoxLayout();
            col1->addWidget(new QLabel(QString::fromUtf8("ESP (FAT32):")));
            espSpin_ = new QSpinBox();
            espSpin_->setRange(int(ESP_MIN_MIB), int(ESP_MAX_MIB));
            espSpin_->setValue(int(opts_.espMiB));
            espSpin_->setSuffix(QString::fromUtf8(" MiB"));
            col1->addWidget(espSpin_);
            grid->addLayout(col1);

            auto* col2 = new QVBoxLayout();
            col2->addWidget(new QLabel(QString::fromUtf8(
                "ext4 root (0 = rest of disk):")));
            rootSpin_ = new QSpinBox();
            rootSpin_->setRange(0, int(ROOT_MAX_MIB));
            rootSpin_->setValue(int(opts_.rootMiB));
            rootSpin_->setSuffix(QString::fromUtf8(" MiB"));
            col2->addWidget(rootSpin_);
            grid->addLayout(col2);

            grid->addStretch(1);
            body->addLayout(grid);

            auto* expl = new QLabel(QString::fromUtf8(
                "The ESP only contains the UEFI stub (EFI/BOOT/BOOTX64.EFI); "
                "it is the FIRST partition of the disk because some "
                "firmwares only try the default boot path there. "
                "The kernel, the Lua scripts and loader.conf live in the "
                "ext4 root partition, which the bootloader reads and the "
                "kernel mounts on /."));
            expl->setWordWrap(true);
            expl->setObjectName("pageHint");
            body->addWidget(expl);

            layoutBar_ = new LayoutBar();
            body->addWidget(layoutBar_);

            layoutErr_ = makeErr();
            body->addWidget(layoutErr_);
            body->addStretch(1);

            for (auto* sb : { espSpin_, rootSpin_ }) {
                connect(sb, &QSpinBox::valueChanged, this,
                        [this] { applyNavState(validateLayout()); });
            }

            stack_->addWidget(titledPage(QString::fromUtf8("Partitioning"),
                                         QString::fromUtf8(
                                             "ESP and ext4 root "
                                             "partition sizes"),
                                         body));
        }

        /* ---- 5. Summary ----------------------------------------------- */
        {
            auto* body = new QVBoxLayout();
            body->setSpacing(10);

            summaryText_ = new QLabel("");
            summaryText_->setWordWrap(true);
            summaryText_->setStyleSheet(
                "background: #FFFFFF; border: 1px solid #D9DCE8;"
                "border-radius: 6px; padding: 14px; font-family: "
                "'DejaVu Sans Mono', monospace; font-size: 12px;");
            body->addWidget(summaryText_, 1);

            auto* warn = new QLabel(QString::fromUtf8(
                "⚠  All data on the selected disk will be "
                "permanently erased."));
            warn->setStyleSheet("color: #C4321D; font-weight: 600;");
            body->addWidget(warn);

            confirmBox_ = new QCheckBox(QString::fromUtf8(
                "I understand that the disk will be entirely erased"));
            body->addWidget(confirmBox_);
            connect(confirmBox_, &QCheckBox::toggled, this,
                    [this] { updateNav(); });

            stack_->addWidget(titledPage(QString::fromUtf8("Summary"),
                                         QString::fromUtf8(
                                             "Verify before writing"),
                                         body));
        }

        /* ---- 6. Installation ------------------------------------------ */
        {
            auto* body = new QVBoxLayout();
            body->setSpacing(10);

            stepLabel_ = new QLabel(QString::fromUtf8("Preparing..."));
            stepLabel_->setStyleSheet("font-weight: 600;");
            body->addWidget(stepLabel_);

            bar_ = new QProgressBar();
            bar_->setRange(0, 100);
            bar_->setTextVisible(true);
            bar_->setStyleSheet(
                "QProgressBar { border: 1px solid #D9DCE8; border-radius:"
                " 6px; background: #FFFFFF; height: 22px; text-align:"
                " center; }"
                "QProgressBar::chunk { background: #4653D8;"
                "border-radius: 5px; }");
            body->addWidget(bar_);

            log_ = new QPlainTextEdit();
            log_->setReadOnly(true);
            log_->setStyleSheet(
                "QPlainTextEdit { background: #141A28; color: #C8D2EC;"
                "border-radius: 6px; font-family: 'DejaVu Sans Mono',"
                " monospace; font-size: 11px; }");
            body->addWidget(log_, 1);

            stack_->addWidget(titledPage(QString::fromUtf8("Installation"),
                                         QString::fromUtf8(
                                             "Writing the loader, "
                                             "kernel and partitions"),
                                         body));
        }

        /* ---- 7. Done ---------------------------------------------------- */
        {
            auto* body = new QVBoxLayout();
            body->setSpacing(12);
            doneSummary_ = new QLabel("");
            doneSummary_->setWordWrap(true);
            doneSummary_->setStyleSheet(
                "font-size: 14px; color: #1D7A4F;");
            body->addWidget(doneSummary_);
            body->addStretch(1);
            stack_->addWidget(titledPage(QString::fromUtf8("Done"),
                                         QString(), body));
        }

        if (!opts_.sourceDir.empty()) validateSource();
        if (diskTable_->rowCount() > 0)
            diskTable_->selectRow(0);
        validateDisk();
        validateLayout();
    }

    /* ---------------------------------------------------------------- */
    /* Page validation.                                                  */
    /* ---------------------------------------------------------------- */

    void collectOptions()
    {
        opts_.espMiB = uint64_t(espSpin_->value());
        opts_.rootMiB = uint64_t(rootSpin_->value());
    }

    bool validateSource()
    {
        sourceErr_->setText("");
        sourceOk_->setText("");
        opts_.sourceDir = sourceEdit_->text().trimmed().toStdString();

        if (opts_.sourceDir.empty()) {
            sourceErr_->setText(QString::fromUtf8(
                "Enter the build directory (e.g. ../PrismOS/build)."));
            return false;
        }
        prism::Installer inst(opts_);
        try {
            src_ = inst.loadSource();
            sourceOk_->setText(QString::fromUtf8(
                "✓ loader %1 · kernel %2 · %3 ESP files (%4) · "
                "%5 root files (%6)")
                .arg(QString::fromStdString(humanSize(src_.loader.size())))
                .arg(QString::fromStdString(humanSize(src_.kernelBytes)))
                .arg(src_.espFiles)
                .arg(QString::fromStdString(humanSize(src_.espTreeBytes)))
                .arg(src_.rootFiles)
                .arg(QString::fromStdString(humanSize(src_.rootTreeBytes))));
            return true;
        } catch (const std::exception& ex) {
            sourceErr_->setText(QString::fromStdString(ex.what()));
            src_ = SourceInfo{};
            return false;
        }
    }

    bool validateDisk()
    {
        diskErr_->setText("");
        diskOk_->setText("");

        int row = diskTable_->currentRow();
        QString imagePath = imageEdit_->text().trimmed();
        opts_.imageSize = 0;

        if (!imagePath.isEmpty()) {
            opts_.target = imagePath.toStdString();
            if (!QFile::exists(imagePath))
                opts_.imageSize = uint64_t(imageSizeSpin_->value()) * MIB;
            /* Choosing an image takes precedence over the list. */
        } else if (row >= 0) {
            opts_.target = diskTable_->item(row, 0)->text().toStdString();
        } else {
            opts_.target.clear();
            diskErr_->setText(QString::fromUtf8(
                "Select a disk or enter an image file."));
            return false;
        }

        prism::Installer inst(opts_);
        try {
            target_ = inst.probeTarget();
        } catch (const std::exception& ex) {
            diskErr_->setText(QString::fromStdString(ex.what()));
            target_ = TargetInfo{};
            return false;
        }
        auto problems = inst.safetyProblems(target_);
        if (!problems.empty()) {
            QString all;
            for (const auto& p : problems)
                all += QString::fromStdString(p) + "\n";
            diskErr_->setText(all.trimmed());
            target_ = TargetInfo{};
            return false;
        }
        diskOk_->setText(QString::fromUtf8("✓ %1")
                         .arg(QString::fromStdString(target_.description)));
        validateLayout();
        return true;
    }

    bool validateLayout()
    {
        layoutErr_->setText("");
        collectOptions();
        if (opts_.target.empty() || !src_.espFiles || !src_.rootFiles) {
            layoutBar_->setRegions({});
            return false;
        }

        prism::Installer inst(opts_);
        try {
            plan_ = inst.computePlan(target_, src_);
        } catch (const std::exception& ex) {
            layoutErr_->setText(QString::fromStdString(ex.what()));
            layoutBar_->setRegions({});
            return false;
        }

        QList<LayoutBar::Region> regions;
        auto add = [&](const char* name, uint64_t bytes, QColor color,
                       const QString& tip) {
            LayoutBar::Region r;
            r.name = QString::fromUtf8(name);
            r.bytes = bytes;
            r.color = color;
            r.tooltip = tip;
            regions << r;
        };
        add("GPT", 34 * 512, QColor("#6B7280"),
            QString::fromUtf8("Partition table (start of disk)"));
        add("ESP", plan_.espBytes(), QColor("#4653D8"),
            QString::fromUtf8("FAT32 (FIRST partition): "
                              "EFI/BOOT/BOOTX64.EFI"));
        add("boot", plan_.bootSectors * SECTOR, QColor("#3B4252"),
            QString::fromUtf8("boot1 + BIOS loader (raw sectors)"));
        add("root", plan_.rootBytes(), QColor("#1FA48E"),
            QString::fromUtf8("PrismOS root: ext4 with the kernel, Lua "
                              "scripts and loader.conf (mounted on / by "
                              "the kernel)"));
        layoutBar_->setRegions(regions);

        /* Text summary. */
        QString s;
        s += QString::fromUtf8("Target       : %1 (%2)\n")
                .arg(QString::fromStdString(target_.path))
                .arg(QString::fromStdString(humanSize(target_.bytes)));
        s += QString::fromUtf8("Loader       : boot0 (MBR) + boot1 + BIOS "
                               "loader %1\n")
                .arg(QString::fromStdString(humanSize(src_.loader.size())));
        s += QString::fromUtf8("Kernel       : %1 → ext4 /boot/kernel/kernel\n")
                .arg(QString::fromStdString(humanSize(src_.kernelBytes)));
        s += QString::fromUtf8("P1 ESP FAT32 : %1 @ LBA %2 (label %3) - first\n")
                .arg(QString::fromStdString(humanSize(plan_.espBytes())))
                .arg(plan_.espStart)
                .arg(QString::fromStdString(plan_.espLabel));
        s += QString::fromUtf8("P2 boot      : %1 @ LBA %2\n")
                .arg(QString::fromStdString(
                    humanSize(plan_.bootSectors * SECTOR)))
                .arg(plan_.bootStart);
        s += QString::fromUtf8("P3 ext4 root : %1 @ LBA %2 (label %3)\n")
                .arg(QString::fromStdString(humanSize(plan_.rootBytes())))
                .arg(plan_.rootStart)
                .arg(QString::fromStdString(plan_.rootLabel));
        s += QString::fromUtf8("GPT          : 3 partitions + backup at "
                               "the end of the disk");
        summaryText_->setText(s);
        return true;
    }

    /* ---------------------------------------------------------------- */
    /* Navigation.                                                       */
    /* ---------------------------------------------------------------- */

    void updateNav()
    {
        int i = stack_->currentIndex();
        btnPrev_->setVisible(i > 0 && i < 6);
        btnPrev_->setEnabled(i > 0 && i < 6);

        bool ok = true;
        if (i == 1) ok = validateSource();
        else if (i == 2) ok = validateDisk();
        else if (i == 3) ok = validateLayout();
        else if (i == 4) ok = confirmBox_->isChecked();

        applyNavState(ok);
    }

    /* Applies the "Next" button state for the current page.
     * Also called by the reactive validators (keyboard input,
     * disk selection, sizes, confirmation checkbox): the button
     * follows the page state live, without waiting for a page
     * change. No validator re-run here (updateNav handles
     * that): no recursion. */
    void applyNavState(bool ok)
    {
        int i = stack_->currentIndex();
        if (i == 0) ok = true;         /* Welcome page: always valid */
        if (i == 5) {
            btnNext_->setText(QString::fromUtf8("Finish"));
            btnNext_->setEnabled(false);
        } else if (i == 4) {
            btnNext_->setText(QString::fromUtf8("Install ▸"));
            btnNext_->setEnabled(ok && !installing_);
        } else if (i == 6) {
            btnNext_->setText(QString::fromUtf8("Finish"));
            btnNext_->setEnabled(true);
        } else {
            btnNext_->setText(QString::fromUtf8("Next →"));
            btnNext_->setEnabled(ok && !installing_);
        }
    }

    void prev()
    {
        int i = stack_->currentIndex();
        if (i > 0 && i < 6) {
            stack_->setCurrentIndex(i - 1);
            steps_->setStep(i - 1);
            updateNav();
        }
    }

    void next()
    {
        int i = stack_->currentIndex();
        if (i == 6) { qApp->quit(); return; }
        if (i == 5) return;

        /* Blocking validation before moving forward. */
        if (i == 1 && !validateSource()) return;
        if (i == 2 && !validateDisk()) return;
        if (i == 3 && !validateLayout()) return;
        if (i == 4) {
            if (!confirmBox_->isChecked()) return;
            startInstall();
            return;
        }

        stack_->setCurrentIndex(i + 1);
        steps_->setStep(i + 1);
        updateNav();
    }

    void startInstall()
    {
        collectOptions();
        installing_ = true;
        btnNext_->setEnabled(false);
        btnPrev_->setEnabled(false);
        log_->clear();
        bar_->setValue(0);

        stack_->setCurrentIndex(5);
        steps_->setStep(5);

        doneText_ = QString::fromUtf8(
            "PrismOS is installed on %1.\n\n"
            "  • BIOS: boot0 → boot1 → loader → kernel\n"
            "  • UEFI: EFI/BOOT/BOOTX64.EFI → kernel\n"
            "  • ext4 root \"%2\": %3 (kernel, Lua scripts, loader.conf)\n\n"
            "The disk can be plugged in and booted directly.")
            .arg(QString::fromStdString(target_.path))
            .arg(QString::fromStdString(plan_.rootLabel))
            .arg(QString::fromStdString(humanSize(plan_.rootBytes())));

        auto* thread = new QThread(this);
        auto* worker = new InstallWorker(opts_);
        worker->moveToThread(thread);

        connect(thread, &QThread::started, worker, &InstallWorker::run);
        connect(worker, &InstallWorker::info, this,
                [this](const QString& m) { onInfo(m); });
        connect(worker, &InstallWorker::detail, this,
                [this](const QString& m) { onDetail(m); });
        connect(worker, &InstallWorker::progress, this,
                [this](int p, const QString& s) { onProgress(p, s); });
        connect(worker, &InstallWorker::finished, this,
                [this, thread, worker] {
                    onFinished();
                    thread->quit();
                    connect(thread, &QThread::finished, worker,
                            &QObject::deleteLater);
                });
        connect(worker, &InstallWorker::failed, this,
                [this, thread, worker](const QString& e) {
                    onFailed(e);
                    thread->quit();
                    connect(thread, &QThread::finished, worker,
                            &QObject::deleteLater);
                });
        connect(thread, &QThread::finished, thread,
                &QObject::deleteLater);

        thread->start();
    }

    /* ---------------------------------------------------------------- */
    /* Self-test: drives the wizard without interaction (offscreen).    */
    /* ---------------------------------------------------------------- */

public:
    void selfTest(const InstallOptions& opts)
    {
        selfTestActive_ = true;

        /* Safety net: after 90 s, assume a hang. */
        QTimer::singleShot(90000, qApp, [] {
            qApp->exit(4);
        });

        /* 1. Source. */
        stack_->setCurrentIndex(1);
        steps_->setStep(1);
        updateNav();
        /* Reactivity of the "Next" button (regression): entering
         * an invalid path must disable it WITHOUT changing
         * pages, a valid path re-enables it. */
        sourceEdit_->setText(QString::fromStdString(opts.sourceDir));
        if (!validateSource()) { qApp->exit(2); return; }
        updateNav();
        if (!btnNext_->isEnabled()) { qApp->exit(5); return; }
        sourceEdit_->setText("/path/that/does/not/exist");
        if (btnNext_->isEnabled()) { qApp->exit(5); return; }
        sourceEdit_->setText(QString::fromStdString(opts.sourceDir));
        if (!btnNext_->isEnabled()) { qApp->exit(5); return; }
        stack_->setCurrentIndex(2);
        steps_->setStep(2);

        /* 2. Target: image to create. */
        imageEdit_->setText(QString::fromStdString(opts.target));
        imageSizeSpin_->setValue(int(opts.imageSize / MIB));
        if (!validateDisk()) { qApp->exit(2); return; }
        /* The valid image must have enabled the button via the
         * textChanged signal (page still displayed). */
        if (!btnNext_->isEnabled()) { qApp->exit(5); return; }
        stack_->setCurrentIndex(3);
        steps_->setStep(3);

        /* 3. Sizes. */
        espSpin_->setValue(int(opts.espMiB));
        rootSpin_->setValue(int(opts.rootMiB));
        if (!validateLayout()) { qApp->exit(2); return; }
        if (!btnNext_->isEnabled()) { qApp->exit(5); return; }
        stack_->setCurrentIndex(4);
        steps_->setStep(4);

        /* 4. Confirmation + installation. */
        confirmBox_->setChecked(true);
        startInstall();
    }

    bool selfTestActive_ = false;

private:
    /* ---- state ---- */
    prism::cli::Args args_;
    InstallOptions opts_;
    SourceInfo src_;
    TargetInfo target_;
    PartitionPlan plan_;
    bool installing_ = false;
    QString doneText_;

    StepsPanel* steps_ = nullptr;
    QStackedWidget* stack_ = nullptr;
    QPushButton* btnPrev_ = nullptr;
    QPushButton* btnNext_ = nullptr;

    QLineEdit* sourceEdit_ = nullptr;
    QLabel* sourceErr_ = nullptr;
    QLabel* sourceOk_ = nullptr;

    QTableWidget* diskTable_ = nullptr;
    QLineEdit* imageEdit_ = nullptr;
    QSpinBox* imageSizeSpin_ = nullptr;
    QLabel* diskErr_ = nullptr;
    QLabel* diskOk_ = nullptr;

    QSpinBox* espSpin_ = nullptr;
    QSpinBox* rootSpin_ = nullptr;
    LayoutBar* layoutBar_ = nullptr;
    QLabel* layoutErr_ = nullptr;

    QLabel* summaryText_ = nullptr;
    QCheckBox* confirmBox_ = nullptr;

    QLabel* stepLabel_ = nullptr;
    QProgressBar* bar_ = nullptr;
    QPlainTextEdit* log_ = nullptr;
    QLabel* doneSummary_ = nullptr;
};

/* ================================================================== */

#include "Gui.moc"

namespace prism::gui {

int guiMain(int argc, char** argv, const prism::cli::Args& args)
{
    int argc2 = argc;
    QApplication app(argc2, argv);
    app.setApplicationName(PRISM_INSTALLER_NAME);
    app.setApplicationVersion(PRISM_INSTALLER_VERSION);

    InstallWizard wizard(args);
    wizard.show();

    if (args.guiSelfTest) {
        QTimer::singleShot(300, &wizard,
                           [&wizard, opts = args.options]() {
                               wizard.selfTest(opts);
                           });
    }

    return app.exec();
}

} // namespace prism::gui

<?php

use Illuminate\Database\Migrations\Migration;
use Illuminate\Database\Schema\Blueprint;
use Illuminate\Support\Facades\DB;
use Illuminate\Support\Facades\Schema;

return new class extends Migration
{
    /**
     * Run the migrations.
     *
     * @return void
     */
    public function up()
    {
        //画像
        Schema::create('deleted_files', function (Blueprint $table) {
            $table->id();

            $table->bigInteger("files_id");
            $table->string("name_md5", 32)->comment("ファイル名 兼 MD5値");
            $table->string("file_path",1023)->comment("ストレージ領域へのパス(削除済み)");
            $table->timestamp('files_created_at');
            $table->timestamp('files_updated_at');

            $table->timestamp('created_at')->default(DB::raw('CURRENT_TIMESTAMP'));
            $table->timestamp('updated_at')->default(DB::raw('CURRENT_TIMESTAMP on update CURRENT_TIMESTAMP'));
        });

    }

    /**
     * Reverse the migrations.
     *
     * @return void
     */
    public function down()
    {
        Schema::dropIfExists('deleted_files');
    }

};
